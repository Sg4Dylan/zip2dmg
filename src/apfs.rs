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

use {
    crate::{
        hfs::{categorize_and_sort_entries, estimate_image_size, find_app_icon, SortedEntries},
        zip_reader::ZipEntries,
        DmgDecorations,
    },
    log::{info, warn},
    std::{collections::HashMap, path::PathBuf},
};

const BLOCK_SIZE: u32 = 4096;

const ROOT_INODE: u64 = 2;

const MIN_BLOCKS: u64 = 64;

pub fn build_apfs_image_to_file<R: std::io::Read + std::io::Seek>(
    volume_name: &str,
    entries: &mut ZipEntries<R>,
    decorations: &DmgDecorations,
) -> anyhow::Result<PathBuf> {
    let estimated_size = estimate_image_size(entries);
    let mut image_size = estimated_size;

    let max_retries = 3;
    for attempt in 0..=max_retries {
        match try_build_apfs_to_file(volume_name, entries, image_size, decorations) {
            Ok(path) => return Ok(path),
            Err(e) => {
                let err_str = format!("{}", e);
                let is_out_of_space = err_str.contains("no free blocks")
                    || err_str.contains("out of blocks")
                    || err_str.contains("allocation failed")
                    || err_str.contains("block allocation")
                    || err_str.contains("not enough space");

                if is_out_of_space && attempt < max_retries {
                    let new_size = image_size * 2;
                    warn!(
                        "APFS build failed at {} MB (attempt {}), retrying at {} MB",
                        image_size / (1024 * 1024),
                        attempt + 1,
                        new_size / (1024 * 1024),
                    );
                    image_size = new_size;
                } else {
                    return Err(e.context(format!(
                        "APFS build failed after {} attempts (image size {} MB, estimated {} MB)",
                        attempt + 1,
                        image_size / (1024 * 1024),
                        estimated_size / (1024 * 1024),
                    )));
                }
            }
        }
    }

    Err(anyhow::anyhow!("APFS build failed after max retries"))
}

fn try_build_apfs_to_file<R: std::io::Read + std::io::Seek>(
    volume_name: &str,
    entries: &mut ZipEntries<R>,
    image_size: u64,
    decorations: &DmgDecorations,
) -> anyhow::Result<PathBuf> {
    let sorted = categorize_and_sort_entries(entries);

    let temp_dir = std::env::temp_dir();
    let temp_path = temp_dir.join(format!(
        "zip2dmg-apfs-{}-{}.img",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));

    let total_blocks = (image_size / BLOCK_SIZE as u64).max(MIN_BLOCKS);

    info!(
        "formatting APFS volume '{}' ({} bytes, {} blocks, file-backed at '{}')",
        volume_name,
        image_size,
        total_blocks,
        temp_path.display()
    );

    let mut dev = fstool::block::file::FileBackend::create(&temp_path, image_size)
        .map_err(|e| anyhow::anyhow!("failed to create temp file '{}': {}", temp_path.display(), e))?;

    let mut writer = fstool::fs::apfs::write::ApfsWriter::new(&mut dev, total_blocks, BLOCK_SIZE, volume_name)
        .map_err(|e| anyhow::anyhow!("APFS format failed: {}", e))?;

    let mut dir_oid: HashMap<String, u64> = HashMap::new();
    dir_oid.insert("/".to_string(), ROOT_INODE);

    populate_apfs(&mut writer, entries, &sorted, &mut dir_oid, decorations)?;

    info!("flushing APFS filesystem");
    writer
        .finish()
        .map_err(|e| anyhow::anyhow!("APFS finish failed: {}", e))?;

    drop(dev);

    info!(
        "APFS image written to '{}' ({} bytes)",
        temp_path.display(),
        image_size
    );
    Ok(temp_path)
}

fn populate_apfs<R: std::io::Read + std::io::Seek>(
    writer: &mut fstool::fs::apfs::write::ApfsWriter,
    entries: &mut ZipEntries<R>,
    sorted: &SortedEntries,
    dir_oid: &mut HashMap<String, u64>,
    decorations: &DmgDecorations,
) -> anyhow::Result<()> {
    for dir in &sorted.dirs {
        log::debug!("creating directory: {} (mode={:o})", dir.path, dir.mode);
        let (parent_oid, name) = resolve_parent(dir_oid, &dir.path)?;
        let oid = writer
            .add_dir(parent_oid, &name, dir.mode)
            .map_err(|e| anyhow::anyhow!("APFS add_dir '{}' failed: {}", dir.path, e))?;
        dir_oid.insert(dir.path.clone(), oid);
    }

    for file in &sorted.files {
        log::debug!(
            "creating file: {} ({} bytes, mode={:o})",
            file.path,
            file.size,
            file.mode
        );
        let (parent_oid, name) = resolve_parent(dir_oid, &file.path)?;
        let mut reader = entries
            .read_file(file.zip_index)
            .map_err(|e| anyhow::anyhow!("failed to open ZIP entry for '{}': {}", file.path, e))?;
        writer
            .add_file_from_reader(parent_oid, &name, file.mode, &mut reader, file.size)
            .map_err(|e| anyhow::anyhow!("APFS add_file_from_reader '{}' failed: {}", file.path, e))?;
    }

    for sym in &sorted.symlinks {
        log::debug!(
            "creating symlink: {} -> {} (mode={:o})",
            sym.path,
            sym.target,
            sym.mode
        );
        let (parent_oid, name) = resolve_parent(dir_oid, &sym.path)?;
        writer
            .add_symlink(parent_oid, &name, sym.mode, &sym.target)
            .map_err(|e| anyhow::anyhow!("APFS add_symlink '{}' failed: {}", sym.path, e))?;
    }

    info!(
        "populated APFS: {} directories, {} files, {} symlinks",
        sorted.dirs.len(),
        sorted.files.len(),
        sorted.symlinks.len(),
    );

    write_decorations(writer, entries, dir_oid, decorations)?;

    Ok(())
}

fn write_decorations<R: std::io::Read + std::io::Seek>(
    writer: &mut fstool::fs::apfs::write::ApfsWriter,
    entries: &mut ZipEntries<R>,
    dir_oid: &mut HashMap<String, u64>,
    decorations: &DmgDecorations,
) -> anyhow::Result<()> {
    if decorations.app_drop_link().is_some() {
        info!("creating /Applications symlink (drag-install)");
        writer
            .add_symlink(ROOT_INODE, "Applications", 0o755, "/Applications")
            .map_err(|e| anyhow::anyhow!("APFS add_symlink '/Applications' failed: {}", e))?;
    }

    if decorations.ql_drop_link().is_some() {
        info!("creating /QuickLook symlink");
        writer
            .add_symlink(ROOT_INODE, "QuickLook", 0o755, "/Library/QuickLook")
            .map_err(|e| anyhow::anyhow!("APFS add_symlink '/QuickLook' failed: {}", e))?;
    }

    if let Some((src, filename)) = decorations.background() {
        info!("creating /.background/ directory");
        let bg_dir_oid = writer
            .add_dir(ROOT_INODE, ".background", 0o755)
            .map_err(|e| anyhow::anyhow!("APFS add_dir '/.background' failed: {}", e))?;

        let bg_data = std::fs::read(src)
            .map_err(|e| anyhow::anyhow!("failed to read background image '{}': {}", src.display(), e))?;
        let bg_name = filename.as_str();
        info!("writing background image to /.background/{} ({} bytes)", bg_name, bg_data.len());
        let mut cursor = std::io::Cursor::new(&bg_data);
        writer
            .add_file_from_reader(bg_dir_oid, bg_name, 0o644, &mut cursor, bg_data.len() as u64)
            .map_err(|e| anyhow::anyhow!("APFS add_file_from_reader '/.background/{}' failed: {}", bg_name, e))?;
    }

    let volicon_written = if let Some(src) = decorations.volicon() {
        let icon_data = std::fs::read(src)
            .map_err(|e| anyhow::anyhow!("failed to read volume icon '{}': {}", src.display(), e))?;
        info!(
            "writing volume icon from --volume-icon to /.VolumeIcon.icns ({} bytes)",
            icon_data.len()
        );
        let mut cursor = std::io::Cursor::new(&icon_data);
        writer
            .add_file_from_reader(ROOT_INODE, ".VolumeIcon.icns", 0o644, &mut cursor, icon_data.len() as u64)
            .map_err(|e| anyhow::anyhow!("APFS add_file_from_reader '/.VolumeIcon.icns' failed: {}", e))?;
        true
    } else if let Some((zip_index, rel_path, size)) = find_app_icon(entries) {
        let rel_path_str = rel_path.to_string_lossy().to_string();
        info!(
            "auto-extracting volume icon from ZIP entry '{}' to /.VolumeIcon.icns ({} bytes)",
            rel_path_str, size
        );
        let mut reader = entries
            .read_file(zip_index)
            .map_err(|e| anyhow::anyhow!("failed to open ZIP entry for '{}': {}", rel_path_str, e))?;
        writer
            .add_file_from_reader(ROOT_INODE, ".VolumeIcon.icns", 0o644, &mut reader, size)
            .map_err(|e| anyhow::anyhow!("APFS add_file_from_reader '/.VolumeIcon.icns' failed: {}", e))?;
        true
    } else {
        false
    };

    if volicon_written {
        // FinderInfo xattr (com.apple.FinderInfo, 32 bytes) for the root
        // directory, marking the volume as having a custom icon. Reverse
        // engineering of apfs.kext and fsck_apfs shows the kernel never
        // parses this payload — it is a pure Finder user-space convention —
        // so the exact byte layout only needs to match what macOS
        // `hdiutil`-produced reference DMGs carry.
        //
        // The 32-byte data (only the kHasCustomIcon-related word at offset
        // 8 is set; `add_xattr` prepends its own flags+size header, so we
        // pass the raw payload here):
        //   00 00 00 00 00 00 00 00 04 00 00 00 00 00 00 00
        //   00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
        let finder_info: [u8; 32] = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        writer
            .add_xattr(ROOT_INODE, "com.apple.FinderInfo", &finder_info)
            .map_err(|e| anyhow::anyhow!("APFS add_xattr 'com.apple.FinderInfo' failed: {}", e))?;
        info!("set FinderInfo xattr on root folder (matching reference DMG layout)");
    }

    for (src, target) in decorations.extra_files() {
        let data = std::fs::read(src)
            .map_err(|e| anyhow::anyhow!("failed to read extra file '{}': {}", src.display(), e))?;
        info!("writing extra file to {} ({} bytes)", target, data.len());

        let (parent_oid, name) = resolve_parent_for_target(dir_oid, writer, target)?;
        let mut cursor = std::io::Cursor::new(&data);
        writer
            .add_file_from_reader(parent_oid, &name, 0o644, &mut cursor, data.len() as u64)
            .map_err(|e| anyhow::anyhow!("APFS add_file_from_reader '{}' failed: {}", target, e))?;
    }

    if decorations.needs_ds_store() {
        info!("generating .DS_Store for drag-install experience");
        let ds_store_data = crate::ds_store::DsStoreBuilder::new()
            .apply_decorations(decorations, &entries.root_name)
            .build()?;
        info!("writing .DS_Store ({} bytes)", ds_store_data.len());
        let mut cursor = std::io::Cursor::new(&ds_store_data);
        writer
            .add_file_from_reader(ROOT_INODE, ".DS_Store", 0o644, &mut cursor, ds_store_data.len() as u64)
            .map_err(|e| anyhow::anyhow!("APFS add_file_from_reader '/.DS_Store' failed: {}", e))?;
    }

    Ok(())
}

fn resolve_parent_for_target(
    dir_oid: &mut HashMap<String, u64>,
    writer: &mut fstool::fs::apfs::write::ApfsWriter,
    target: &str,
) -> anyhow::Result<(u64, String)> {
    let (parent_path, name) = split_parent_and_name(target);
    if parent_path == "/" || parent_path.is_empty() {
        return Ok((ROOT_INODE, name));
    }
    if let Some(&oid) = dir_oid.get(&parent_path) {
        return Ok((oid, name));
    }
    let components: Vec<&str> = parent_path.split('/').filter(|c| !c.is_empty()).collect();
    let mut current = String::new();
    let mut current_oid = ROOT_INODE;
    for component in components {
        current = format!("{}/{}", current, component);
        if let Some(&oid) = dir_oid.get(&current) {
            current_oid = oid;
            continue;
        }
        let oid = writer
            .add_dir(current_oid, component, 0o755)
            .map_err(|e| anyhow::anyhow!("APFS add_dir '{}' failed: {}", current, e))?;
        dir_oid.insert(current.clone(), oid);
        current_oid = oid;
    }
    Ok((current_oid, name))
}

fn resolve_parent(
    dir_oid: &HashMap<String, u64>,
    path: &str,
) -> anyhow::Result<(u64, String)> {
    let (parent_path, name) = split_parent_and_name(path);
    let parent_oid = dir_oid
        .get(&parent_path)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("APFS parent directory '{}' not found for '{}'", parent_path, path))?;
    Ok((parent_oid, name))
}

fn split_parent_and_name(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(0) => ("/".to_string(), path[1..].to_string()),
        Some(idx) => (path[..idx].to_string(), path[idx + 1..].to_string()),
        None => ("/".to_string(), path.to_string()),
    }
}
