// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! HFS+ disk image builder from ZIP entries.
//!
//! This module provides [build_hfs_plus_image_to_file] which takes [ZipEntries]
//! and produces a raw HFS+ disk image in a temporary file using
//! `fstool::block::file::FileBackend`. The file-based approach avoids the
//! memory ceiling of [fstool::block::memory::MemoryBackend] (which is designed
//! for unit tests and carries a 256 MiB soft cap) and works efficiently for
//! images of any size.
//!
//! # Why not MemoryBackend?
//!
//! `fstool::block::memory::MemoryBackend` allocates a `Vec<u8>` equal to the
//! entire image size. For a typical Electron .app bundle, the HFS+ image can
//! exceed 500 MiB. Holding this in RAM — on top of the ZIP data and the
//! decompressed file entries already in memory — would double or triple peak
//! memory usage. `FileBackend` writes directly to a temp file, keeping RAM
//! usage proportional to the working set (one file at a time) rather than the
//! total image size.

use {
    crate::zip_reader::{ZipEntries, ZipEntry},
    crate::DmgDecorations,
    log::{info, warn},
    std::path::PathBuf,
};

pub(crate) struct DirEntry {
    pub(crate) path: String,
    pub(crate) mode: u16,
}

pub(crate) struct FileEntry {
    pub(crate) path: String,
    pub(crate) zip_index: usize,
    pub(crate) size: u64,
    pub(crate) mode: u16,
}

pub(crate) struct SymlinkEntry {
    pub(crate) path: String,
    pub(crate) target: String,
    pub(crate) mode: u16,
}

pub(crate) struct SortedEntries {
    pub(crate) dirs: Vec<DirEntry>,
    pub(crate) files: Vec<FileEntry>,
    pub(crate) symlinks: Vec<SymlinkEntry>,
}

/// Build an HFS+ disk image from ZIP entries, writing to a temporary file.
///
/// Returns the path to the temporary file containing the raw HFS+ disk image.
/// The caller is responsible for deleting the file when done (or passing it
/// to a streaming consumer like `DmgWriter::add_partition_file`).
///
/// If the initial size estimate proves too small (HFS+ allocator runs out
/// of blocks during build), the function automatically retries with a
/// larger image. This handles edge cases where the estimate under-counts
/// metadata overhead.
pub fn build_hfs_plus_image_to_file<R: std::io::Read + std::io::Seek>(
    volume_name: &str,
    entries: &mut ZipEntries<R>,
    decorations: &DmgDecorations,
) -> anyhow::Result<PathBuf> {
    let estimated_size = estimate_image_size(entries);
    let mut image_size = estimated_size;

    // Retry loop: if the build fails due to out-of-space, double the size.
    let max_retries = 3;
    for attempt in 0..=max_retries {
        match try_build_hfs_plus_to_file(volume_name, entries, image_size, decorations) {
            Ok(path) => return Ok(path),
            Err(e) => {
                let err_str = format!("{}", e);
                // Check if the error is an out-of-space condition.
                let is_out_of_space = err_str.contains("no free blocks")
                    || err_str.contains("out of blocks")
                    || err_str.contains("allocation failed")
                    || err_str.contains("block allocation")
                    || err_str.contains("HFS+ format failed");

                if is_out_of_space && attempt < max_retries {
                    let new_size = image_size * 2;
                    warn!(
                        "HFS+ build failed at {} MB (attempt {}), retrying at {} MB",
                        image_size / (1024 * 1024),
                        attempt + 1,
                        new_size / (1024 * 1024),
                    );
                    image_size = new_size;
                } else {
                    return Err(e.context(format!(
                        "HFS+ build failed after {} attempts (image size {} MB, estimated {} MB)",
                        attempt + 1,
                        image_size / (1024 * 1024),
                        estimated_size / (1024 * 1024),
                    )));
                }
            }
        }
    }

    Err(anyhow::anyhow!("HFS+ build failed after max retries"))
}

fn try_build_hfs_plus_to_file<R: std::io::Read + std::io::Seek>(
    volume_name: &str,
    entries: &mut ZipEntries<R>,
    image_size: u64,
    decorations: &DmgDecorations,
) -> anyhow::Result<PathBuf> {
    let sorted = categorize_and_sort_entries(entries);

    let temp_dir = std::env::temp_dir();
    let temp_path = temp_dir.join(format!("zip2dmg-{}-{}.img", std::process::id(), std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()));

    info!(
        "formatting HFS+ volume '{}' ({} bytes, file-backed at '{}')",
        volume_name,
        image_size,
        temp_path.display()
    );

    let opts = fstool::fs::hfs_plus::FormatOpts {
        volume_name: volume_name.to_string(),
        block_size: BLOCK_SIZE,
        node_size: NODE_SIZE,
        catalog_nodes: estimate_catalog_nodes(entries),
        extents_nodes: 32,
        create_date: current_hfs_date(),
        journaled: false,
        owner_uid: DEFAULT_UID,
        owner_gid: DEFAULT_GID,
    };

    let mut dev = fstool::block::file::FileBackend::create(&temp_path, image_size)
        .map_err(|e| anyhow::anyhow!("failed to create temp file '{}': {}", temp_path.display(), e))?;
    let mut hfs = fstool::fs::hfs_plus::HfsPlus::format(&mut dev, &opts)
        .map_err(|e| anyhow::anyhow!("HFS+ format failed: {}", e))?;

    populate_hfs(&mut hfs, &mut dev, entries, &sorted, decorations)?;

    info!("flushing HFS+ filesystem");
    hfs.flush(&mut dev)
        .map_err(|e| anyhow::anyhow!("HFS+ flush failed: {}", e))?;

    drop(dev);

    info!("HFS+ image written to '{}' ({} bytes)", temp_path.display(), image_size);
    Ok(temp_path)
}

/// Populate an HFS+ filesystem with directories, files, and symlinks.
///
/// File content is streamed from the ZIP archive on demand — only one file
/// is decompressed in memory at a time, flowing through the HFS+ writer's
/// 64 KiB buffer directly into the block device.
fn populate_hfs<R: std::io::Read + std::io::Seek>(
    hfs: &mut fstool::fs::hfs_plus::HfsPlus,
    dev: &mut dyn fstool::block::BlockDevice,
    entries: &mut ZipEntries<R>,
    sorted: &SortedEntries,
    decorations: &DmgDecorations,
) -> anyhow::Result<()> {
    for dir in &sorted.dirs {
        log::debug!("creating directory: {} (mode={:o})", dir.path, dir.mode);
        hfs.create_dir(dev, &dir.path, dir.mode, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_dir '{}' failed: {}", dir.path, e))?;
    }

    for file in &sorted.files {
        log::debug!("creating file: {} ({} bytes, mode={:o})", file.path, file.size, file.mode);
        let mut reader = entries.read_file(file.zip_index)
            .map_err(|e| anyhow::anyhow!("failed to open ZIP entry for '{}': {}", file.path, e))?;
        hfs.create_file(dev, &file.path, &mut reader, file.size, file.mode, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_file '{}' failed: {}", file.path, e))?;
    }

    for sym in &sorted.symlinks {
        log::debug!("creating symlink: {} -> {} (mode={:o})", sym.path, sym.target, sym.mode);
        hfs.create_symlink(dev, &sym.path, &sym.target, sym.mode, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_symlink '{}' failed: {}", sym.path, e))?;
    }

    info!(
        "populated HFS+: {} directories, {} files, {} symlinks",
        sorted.dirs.len(),
        sorted.files.len(),
        sorted.symlinks.len(),
    );

    // --- HFS+ metadata directories ---

    info!("creating HFS+ Private Data directory");
    hfs.ensure_private_data_dir()
        .map_err(|e| anyhow::anyhow!("HFS+ ensure_private_data_dir failed: {}", e))?;

    info!("creating .HFS+ Private Directory Data directory");
    hfs.create_dir(dev, "/.HFS+ Private Directory Data", 0o700, DEFAULT_UID, DEFAULT_GID, 0)
        .map_err(|e| anyhow::anyhow!("HFS+ create_dir '/.HFS+ Private Directory Data' failed: {}", e))?;

    write_decorations(hfs, dev, entries, decorations)?;

    Ok(())
}

/// Write decoration files (symlinks, background, volicon, extras, .DS_Store)
/// to the HFS+ filesystem.
fn write_decorations<R: std::io::Read + std::io::Seek>(
    hfs: &mut fstool::fs::hfs_plus::HfsPlus,
    dev: &mut dyn fstool::block::BlockDevice,
    entries: &mut ZipEntries<R>,
    decorations: &DmgDecorations,
) -> anyhow::Result<()> {
    if decorations.app_drop_link().is_some() {
        info!("creating /Applications symlink (drag-install)");
        hfs.create_symlink(dev, "/Applications", "/Applications", 0o755, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_symlink '/Applications' failed: {}", e))?;
    }

    if decorations.ql_drop_link().is_some() {
        info!("creating /QuickLook symlink");
        hfs.create_symlink(dev, "/QuickLook", "/Library/QuickLook", 0o755, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_symlink '/QuickLook' failed: {}", e))?;
    }

    if let Some((src, filename)) = decorations.background() {
        info!("creating /.background/ directory");
        hfs.create_dir(dev, "/.background", 0o755, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_dir '/.background' failed: {}", e))?;

        let bg_data = std::fs::read(src)
            .map_err(|e| anyhow::anyhow!("failed to read background image '{}': {}", src.display(), e))?;
        let bg_path = format!("/.background/{}", filename);
        info!("writing background image to {} ({} bytes)", bg_path, bg_data.len());
        let mut cursor = std::io::Cursor::new(&bg_data);
        hfs.create_file(dev, &bg_path, &mut cursor, bg_data.len() as u64, 0o644, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_file '{}' failed: {}", bg_path, e))?;
    }

    // --- Volume icon ---
    // Priority: 1) explicit --volume-icon from CLI  2) auto-extract from app's Info.plist → CFBundleIconFile
    // For auto-extract, find the first .icns file under Contents/Resources/ in the ZIP.
    let volicon_written = if let Some(src) = decorations.volicon() {
        let icon_data = std::fs::read(src)
            .map_err(|e| anyhow::anyhow!("failed to read volume icon '{}': {}", src.display(), e))?;
        info!("writing volume icon from --volume-icon to /.VolumeIcon.icns ({} bytes)", icon_data.len());
        let mut cursor = std::io::Cursor::new(&icon_data);
        hfs.create_file(dev, "/.VolumeIcon.icns", &mut cursor, icon_data.len() as u64, 0o644, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_file '/.VolumeIcon.icns' failed: {}", e))?;
        true
    } else if let Some((zip_index, rel_path, size)) = find_app_icon(entries) {
        let rel_path_str = rel_path.to_string_lossy().to_string();
        info!("auto-extracting volume icon from ZIP entry '{}' to /.VolumeIcon.icns ({} bytes)", rel_path_str, size);
        let mut reader = entries.read_file(zip_index)
            .map_err(|e| anyhow::anyhow!("failed to open ZIP entry for '{}': {}", rel_path_str, e))?;
        hfs.create_file(dev, "/.VolumeIcon.icns", &mut reader, size, 0o644, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_file '/.VolumeIcon.icns' failed: {}", e))?;
        true
    } else {
        false
    };

    if volicon_written {
        const K_HAS_CUSTOM_ICON: u16 = 0x0400;
        hfs.set_folder_finder_flags(2, K_HAS_CUSTOM_ICON)
            .map_err(|e| anyhow::anyhow!("HFS+ set_folder_finder_flags failed: {}", e))?;
        info!("set kHasCustomIcon flag on root folder");
    }

    for (src, target) in decorations.extra_files() {
        let data = std::fs::read(src)
            .map_err(|e| anyhow::anyhow!("failed to read extra file '{}': {}", src.display(), e))?;
        info!("writing extra file to {} ({} bytes)", target, data.len());
        let mut cursor = std::io::Cursor::new(&data);

        if let Some(parent) = target.rfind('/') {
            let parent_path = &target[..parent];
            if !parent_path.is_empty() && parent_path != "/" {
                create_dir_recursive(hfs, dev, parent_path)?;
            }
        }

        hfs.create_file(dev, target, &mut cursor, data.len() as u64, 0o644, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_file '{}' failed: {}", target, e))?;
    }

    if decorations.needs_ds_store() {
        info!("generating .DS_Store for drag-install experience");
        let ds_store_data = crate::ds_store::DsStoreBuilder::new()
            .apply_decorations(decorations, &entries.root_name)
            .build()?;
        info!("writing .DS_Store ({} bytes)", ds_store_data.len());
        let mut cursor = std::io::Cursor::new(&ds_store_data);
        hfs.create_file(dev, "/.DS_Store", &mut cursor, ds_store_data.len() as u64, 0o644, DEFAULT_UID, DEFAULT_GID, 0)
            .map_err(|e| anyhow::anyhow!("HFS+ create_file '/.DS_Store' failed: {}", e))?;
    }

    Ok(())
}

/// Create directories recursively in the HFS+ filesystem.
fn create_dir_recursive(
    hfs: &mut fstool::fs::hfs_plus::HfsPlus,
    dev: &mut dyn fstool::block::BlockDevice,
    path: &str,
) -> anyhow::Result<()> {
    // Split path into components and create each level if needed.
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let mut current = String::new();
    for component in components {
        current = format!("{}/{}", current, component);
        // Try to create the directory; ignore "already exists" errors.
        match hfs.create_dir(dev, &current, 0o755, DEFAULT_UID, DEFAULT_GID, 0) {
            Ok(_) => log::debug!("created directory: {}", current),
            Err(e) => {
                // fstool returns "already exists" as a formatted string, not a
                // structured error variant. Match on the message text as a
                // workaround — see fstool hfs_plus::writer for the source.
                let err_str = format!("{}", e);
                if err_str.contains("already exists") {
                    // Directory already exists, which is fine.
                } else {
                    return Err(anyhow::anyhow!("HFS+ create_dir '{}' failed: {}", current, e));
                }
            }
        }
    }
    Ok(())
}

/// HFS+ allocation block size used by [build_hfs_plus_image_to_file].
const BLOCK_SIZE: u32 = 4096;

/// HFS+ B-tree node size used by [build_hfs_plus_image_to_file].
const NODE_SIZE: u32 = 8192;

/// Default UID for files in the DMG (first macOS user).
pub(crate) const DEFAULT_UID: u32 = 501;

/// Default GID for files in the DMG (macOS "staff" group).
pub(crate) const DEFAULT_GID: u32 = 20;

/// Minimum HFS+ image size: enough for volume header + a few B-tree nodes.
const MIN_IMAGE_SIZE: u64 = 16 * 1024 * 1024;

/// Alignment boundary for image size rounding (8 MiB).
///
/// macOS `hdiutil` typically aligns HFS+ image sizes to 8 MiB boundaries.
/// Using 8 MiB alignment ensures the image is large enough to accommodate
/// metadata overhead that the simple estimator may under-count (e.g. B-tree
/// growth, hot-file B-tree, attribute B-tree, extended attributes).
const ALIGN_SIZE: u64 = 8 * 1024 * 1024;

/// Catalog B-tree estimation constants.
const RECORDS_PER_LEAF: u32 = 20;
const RECORDS_PER_INDEX: u32 = 256;

/// HFS+ epoch offset: seconds between 1904-01-01 and 1970-01-01.
const HFS_EPOCH_OFFSET: u32 = 2082844800;

/// Find the app's icon file in the ZIP entries.
///
/// Searches for `.icns` files under `Contents/Resources/` in the app bundle.
/// macOS app bundles store their icon at `Contents/Resources/<icon>.icns`,
/// where `<icon>` is specified in `Info.plist` as `CFBundleIconFile`.
/// Since we don't parse Info.plist here, we use a heuristic: prefer files
/// whose name contains "app" or "icon", otherwise use the first `.icns` found.
pub(crate) fn find_app_icon<R: std::io::Read + std::io::Seek>(entries: &ZipEntries<R>) -> Option<(usize, &PathBuf, u64)> {
    let mut first_icns: Option<(usize, &PathBuf, u64)> = None;
    let mut preferred_icns: Option<(usize, &PathBuf, u64)> = None;

    for item in &entries.items {
        let ZipEntry::File { relative_path, zip_index, size, .. } = item else { continue };
        let path_str = relative_path.to_string_lossy();

        if !path_str.contains("Contents/Resources/") {
            continue;
        }
        if !path_str.to_ascii_lowercase().ends_with(".icns") {
            continue;
        }

        let filename = path_str.rsplit('/').next().unwrap_or("").to_ascii_lowercase();

        if first_icns.is_none() {
            first_icns = Some((*zip_index, relative_path, *size));
        }

        if filename.contains("app") || filename.contains("icon") {
            preferred_icns = Some((*zip_index, relative_path, *size));
            break;
        }
    }

    preferred_icns.or(first_icns)
}

/// Calculate catalog B-tree node counts from entry count.
///
/// Returns (leaf_nodes, index_nodes, total_nodes) where total_nodes
/// includes the header node.
fn calc_catalog_node_counts(entry_count: u32) -> (u32, u32, u32) {
    let catalog_records = (entry_count + 1) * 2;
    let leaf_nodes = (catalog_records + RECORDS_PER_LEAF - 1) / RECORDS_PER_LEAF;
    let index_nodes = if leaf_nodes <= 1 { 0 } else { (leaf_nodes + RECORDS_PER_INDEX - 1) / RECORDS_PER_INDEX };
    let total_nodes = 1 + index_nodes + leaf_nodes;
    (leaf_nodes, index_nodes, total_nodes)
}

/// Estimate the total HFS+ image size needed for the ZIP entries.
///
/// The model accounts for:
/// - File data, rounded up to whole allocation blocks (4 KiB each)
/// - Catalog B-tree: each entry produces ~2 records (item + thread);
///   each 8 KiB leaf node holds ~20 records; plus index/header nodes
/// - Allocation bitmap: 1 bit per allocation block
/// - Volume header: 1 block
/// - Extents-overflow B-tree: a small reserved area
/// - 10% free space reserve for B-tree growth and alignment slack
pub(crate) fn estimate_image_size<R: std::io::Read + std::io::Seek>(entries: &ZipEntries<R>) -> u64 {
    let block_size = BLOCK_SIZE as u64;

    let data_blocks: u64 = entries.items.iter().map(|e| {
        match e {
            ZipEntry::File { size, .. } => (*size + block_size - 1) / block_size,
            _ => 0,
        }
    }).sum();

    let data_bytes = data_blocks * block_size;

    let entry_count = entries.items.len() as u64;
    let (_, _, catalog_nodes) = calc_catalog_node_counts(entry_count as u32);
    let catalog_nodes = catalog_nodes as u64;
    let catalog_bytes = catalog_nodes * NODE_SIZE as u64;

    let extents_bytes = 4 * NODE_SIZE as u64;

    let vh_blocks = 1u64;

    let metadata_blocks = (catalog_bytes + extents_bytes + block_size - 1) / block_size;
    let total_blocks_pass1 = data_blocks + metadata_blocks + vh_blocks;

    // Allocation bitmap: 1 bit per block, rounded up to whole blocks.
    let bitmap_bytes = (total_blocks_pass1 + 7) / 8;
    let bitmap_blocks = (bitmap_bytes + block_size - 1) / block_size;

    let alt_vh_blocks = 1u64;

    let total_blocks = total_blocks_pass1 + bitmap_blocks + alt_vh_blocks;

    let free_blocks = (total_blocks / 10).max(16);
    let final_blocks = total_blocks + free_blocks;
    let final_bytes = final_blocks * block_size;

    let aligned = (final_bytes.max(MIN_IMAGE_SIZE) + ALIGN_SIZE - 1) & !(ALIGN_SIZE - 1);

    log::info!(
        "estimated image size: {} MB (data={} MB, entries={}, catalog_nodes={}, bitmap_blocks={})",
        aligned / (1024 * 1024),
        data_bytes / (1024 * 1024),
        entry_count,
        catalog_nodes,
        bitmap_blocks,
    );

    aligned
}

/// Estimate the number of catalog B-tree nodes needed.
///
/// Each entry produces ~2 catalog records (record + thread).
/// Each 8 KiB leaf node holds ~20 records. We add headroom for
/// index nodes and the header node.
fn estimate_catalog_nodes<R: std::io::Read + std::io::Seek>(entries: &ZipEntries<R>) -> u32 {
    let entry_count = entries.items.len() as u32;
    let (_, _, base_nodes) = calc_catalog_node_counts(entry_count);
    // Add generous headroom for B-tree growth
    (base_nodes + 8).max(32)
}

/// Get the current time as an HFS+ date (seconds since 1904-01-01).
fn current_hfs_date() -> u32 {
    let unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    unix_seconds.checked_add(HFS_EPOCH_OFFSET).unwrap_or(u32::MAX)
}

/// Categorize and sort entries for HFS+ creation order.
///
/// Directories are sorted by depth (shallowest first).
pub(crate) fn categorize_and_sort_entries<R: std::io::Read + std::io::Seek>(
    entries: &ZipEntries<R>,
) -> SortedEntries {
    let root_name = &entries.root_name;
    let mut dirs: Vec<DirEntry> = Vec::new();
    let mut files: Vec<FileEntry> = Vec::new();
    let mut symlinks: Vec<SymlinkEntry> = Vec::new();

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in &entries.items {
        let hfs_path = if entry.relative_path().as_os_str().is_empty() {
            format!("/{}", root_name)
        } else {
            format!(
                "/{}/{}",
                root_name,
                entry.relative_path().to_string_lossy().replace('\\', "/")
            )
        };

        match entry {
            ZipEntry::Directory { mode, .. } => {
                if seen.insert(hfs_path.clone()) {
                    dirs.push(DirEntry { path: hfs_path, mode: (*mode & 0o7777) as u16 });
                } else {
                    log::debug!("skipping duplicate directory entry: {}", hfs_path);
                }
            }
            ZipEntry::File { zip_index, size, mode, .. } => {
                if seen.insert(hfs_path.clone()) {
                    files.push(FileEntry {
                        path: hfs_path,
                        zip_index: *zip_index,
                        size: *size,
                        mode: (*mode & 0o7777) as u16,
                    });
                } else {
                    log::debug!("skipping duplicate file entry: {}", hfs_path);
                }
            }
            ZipEntry::Symlink { target, mode, .. } => {
                if seen.insert(hfs_path.clone()) {
                    let target_str = target.to_string_lossy().replace('\\', "/");
                    symlinks.push(SymlinkEntry {
                        path: hfs_path,
                        target: target_str,
                        mode: (*mode & 0o7777) as u16,
                    });
                } else {
                    log::debug!("skipping duplicate symlink entry: {}", hfs_path);
                }
            }
        }
    }

    let root_path = format!("/{}", root_name);
    for entry in &entries.items {
        if entry.relative_path().as_os_str().is_empty() {
            continue;
        }
        let rel_str = entry.relative_path().to_string_lossy().replace('\\', "/");
        let mut current = root_path.clone();
        let components: Vec<&str> = rel_str.split('/').filter(|c| !c.is_empty()).collect();
        for component in &components[..components.len().saturating_sub(1)] {
            let dir_path = format!("{}/{}", current, component);
            if seen.insert(dir_path.clone()) {
                dirs.push(DirEntry { path: dir_path, mode: 0o755 });
            }
            current = format!("{}/{}", current, component);
        }
    }

    dirs.sort_by(|a, b| {
        let depth_a = a.path.matches('/').count();
        let depth_b = b.path.matches('/').count();
        depth_a.cmp(&depth_b).then(a.path.cmp(&b.path))
    });

    let root_path = format!("/{}", root_name);
    if !dirs.iter().any(|d| d.path == root_path) {
        dirs.insert(0, DirEntry { path: root_path, mode: 0o755 });
    }

    SortedEntries { dirs, files, symlinks }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip_reader::ZipEntries;
    use std::io::{Cursor, Write};

    /// Build a minimal ZIP containing a `.app` bundle (single file), used
    /// for pure-function tests.
    fn minimal_app_zip() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let dir_opts = zip::write::SimpleFileOptions::default()
                .unix_permissions(0o755)
                .compression_method(zip::CompressionMethod::Stored);
            let file_opts = zip::write::SimpleFileOptions::default()
                .unix_permissions(0o644)
                .compression_method(zip::CompressionMethod::Deflated);
            w.add_directory("T.app/", dir_opts).unwrap();
            w.add_directory("T.app/Contents/", dir_opts).unwrap();
            w.add_directory("T.app/Contents/MacOS/", dir_opts).unwrap();
            w.start_file("T.app/Contents/MacOS/T", file_opts).unwrap();
            w.write_all(b"hello").unwrap();
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn test_estimate_image_size_aligned_and_above_min() {
        let zip_data = minimal_app_zip();
        let entries = ZipEntries::from_zip_data(&zip_data).expect("parse zip");
        let size = estimate_image_size(&entries);

        // Must not fall below the minimum image size
        assert!(size >= MIN_IMAGE_SIZE, "size {size} < MIN_IMAGE_SIZE {MIN_IMAGE_SIZE}");
        // Must be aligned to ALIGN_SIZE
        assert_eq!(size % ALIGN_SIZE, 0, "size {size} is not aligned to ALIGN_SIZE {ALIGN_SIZE}");
        // Must be a multiple of the block size
        assert_eq!(size % BLOCK_SIZE as u64, 0, "size {size} is not a multiple of BLOCK_SIZE");
    }

    #[test]
    fn test_estimate_image_size_grows_with_file_size() {
        // The estimate should grow monotonically with file size
        let small = minimal_app_zip();
        let entries_small = ZipEntries::from_zip_data(&small).unwrap();
        let size_small = estimate_image_size(&entries_small);

        // Build a ZIP containing a large file
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let dir_opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let file_opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.add_directory("T.app/", dir_opts.clone()).unwrap();
            w.add_directory("T.app/Contents/", dir_opts.clone()).unwrap();
            w.add_directory("T.app/Contents/MacOS/", dir_opts).unwrap();
            w.start_file("T.app/Contents/MacOS/T", file_opts).unwrap();
            // Write 1 MiB of data to ensure it spans multiple blocks
            w.write_all(&vec![0u8; 1024 * 1024]).unwrap();
            w.finish().unwrap();
        }
        let entries_big = ZipEntries::from_zip_data(&buf.into_inner()).unwrap();
        let size_big = estimate_image_size(&entries_big);

        assert!(size_big >= size_small, "large-file estimate {size_big} < small-file estimate {size_small}");
    }
}
