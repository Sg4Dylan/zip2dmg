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

//! CLI entry point for `zip2dmg`.

use {
    anyhow::{Context, Result},
    clap::Parser,
    std::{
        fs::File,
        io::{BufReader, BufWriter, Write},
        path::PathBuf,
    },
    zip2dmg::{
        build_hfs_plus_image_to_file,
        gpt::{build_gpt_layout, build_gpt_layout_with_fs, FilesystemKind},
        DmgDecorations, ZipEntries,
    },
};

/// Convert a ZIP archive containing a macOS .app bundle into an unsigned DMG disk image.
#[derive(Parser)]
#[command(name = "zip2dmg", version, about)]
struct Args {
    /// Path to the input ZIP file.
    #[arg(long)]
    input: PathBuf,

    /// Path to the output DMG file.
    #[arg(long)]
    output: PathBuf,

    /// HFS+ volume name for the DMG.
    #[arg(long, default_value = "Untitled")]
    volume_name: String,

    /// Disk image format (compression method for DMG data blocks).
    /// Accepts create-dmg names: UDZO, UDBZ, ULFO, ULMO; or algorithm names:
    /// zlib, bzip2, lzfse, xz, raw/none.
    #[arg(long = "format", alias = "compression", default_value = "UDZO")]
    format: String,

    /// Create an /Applications symlink for drag-install.
    /// Format: "x,y" for icon position, or just "yes" for default position.
    #[arg(long)]
    app_drop_link: Option<String>,

    /// Create a /QuickLook symlink for QuickLook install.
    /// Format: "x,y" for icon position, or just "yes" for default position.
    #[arg(long)]
    ql_drop_link: Option<String>,

    /// Background image to embed in the DMG (for Finder window background).
    #[arg(long)]
    background: Option<PathBuf>,

    /// Volume icon (.icns) to embed in the DMG.
    #[arg(long = "volume-icon")]
    volume_icon: Option<PathBuf>,

    /// Finder window position. Format: "x,y".
    #[arg(long)]
    window_pos: Option<String>,

    /// Finder window size. Format: "width,height".
    #[arg(long)]
    window_size: Option<String>,

    /// Icon size in pixels (default: 100).
    #[arg(long)]
    icon_size: Option<u32>,

    /// Text size in points for icon labels (default: 16, range: 10-16).
    #[arg(long)]
    text_size: Option<u32>,

    /// Position an item's icon. Format: "name,x,y". Can be specified multiple times.
    #[arg(long = "icon")]
    icons: Vec<String>,

    /// Hide the extension of a file in Finder. Can be specified multiple times.
    #[arg(long = "hide-extension")]
    hide_extensions: Vec<String>,

    /// Add an extra file to the DMG. Format: "source_path,target_path" or
    /// "source_path,target_path,x,y" (with icon position). Can be specified multiple times.
    #[arg(long = "add-file")]
    add_files: Vec<String>,

    /// Attach an EULA license file (plain text or RTF) to the DMG.
    /// The user must agree to the license before mounting.
    #[arg(long)]
    eula: Option<PathBuf>,

    /// Disk image filesystem: `HFS+` (default) or `APFS` (macOS 10.13+).
    #[arg(long, default_value = "HFS+")]
    filesystem: String,
}

/// Parse compression method from string.
///
/// Accepts both create-dmg format names and algorithm names:
/// - `UDZO` / `zlib` / `deflate` → Zlib (default, best compatibility)
/// - `UDBZ` / `bzip2` / `bz2`   → Bzip2 (smaller)
/// - `ULFO` / `lzfse`            → LZFSE (smaller and faster, macOS 10.11+)
/// - `ULMO` / `xz` / `lzma` / `lzma2` → LZMA
/// - `raw` / `none`              → No compression
fn parse_compression(s: &str) -> anyhow::Result<udif::CompressionMethod> {
    match s.to_ascii_lowercase().as_str() {
        "raw" | "none" => Ok(udif::CompressionMethod::Raw),
        "udzo" | "zlib" | "deflate" => Ok(udif::CompressionMethod::Zlib),
        "udbz" | "bzip2" | "bz2" => Ok(udif::CompressionMethod::Bzip2),
        "ulfo" | "lzfse" => Ok(udif::CompressionMethod::Lzfse),
        "ulmo" | "xz" | "lzma" | "lzma2" => Ok(udif::CompressionMethod::Xz),
        _ => Err(anyhow::anyhow!(
            "unknown format '{}'; supported: UDZO/zlib, UDBZ/bzip2, ULFO/lzfse, ULMO/xz, raw/none",
            s
        )),
    }
}

/// Parse a "x,y" string into a tuple of i32 values.
fn parse_xy(s: &str) -> anyhow::Result<(i32, i32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!("expected format 'x,y', got '{}'", s));
    }
    let x_str = parts[0].trim();
    let y_str = parts[1].trim();
    let x: i32 = x_str.parse()
        .map_err(|_| anyhow::anyhow!("invalid x value: '{}'", x_str))?;
    let y: i32 = y_str.parse()
        .map_err(|_| anyhow::anyhow!("invalid y value: '{}'", y_str))?;
    Ok((x, y))
}

/// Parse the --app-drop-link or --ql-drop-link value.
/// Accepts "yes" (default position), "x,y" (custom position).
fn parse_drop_link(s: &str, default_pos: (i32, i32)) -> anyhow::Result<(i32, i32)> {
    let lower = s.to_ascii_lowercase();
    if lower == "yes" || lower == "true" || lower == "1" {
        return Ok(default_pos);
    }
    parse_xy(s)
}

/// Build DmgDecorations from CLI arguments.
fn build_decorations(args: &Args) -> anyhow::Result<DmgDecorations> {
    let mut builder = DmgDecorations::builder();

    if let Some(ref val) = args.app_drop_link {
        builder = builder.app_drop_link(parse_drop_link(val, (490, 190))?);
    }

    if let Some(ref val) = args.ql_drop_link {
        builder = builder.ql_drop_link(parse_drop_link(val, (490, 390))?);
    }

    if let Some(ref path) = args.background {
        if !path.exists() {
            return Err(anyhow::anyhow!("background image not found: {}", path.display()));
        }
        let filename = path.file_name()
            .ok_or_else(|| anyhow::anyhow!("invalid background image path: {}", path.display()))?
            .to_string_lossy()
            .to_string();
        builder = builder.background(path.clone(), filename);
    }

    if let Some(ref path) = args.volume_icon {
        if !path.exists() {
            return Err(anyhow::anyhow!("volume icon not found: {}", path.display()));
        }
        builder = builder.volicon(path.clone());
    }

    if let Some(ref val) = args.window_pos {
        builder = builder.window_pos(parse_xy(val)?);
    }

    if let Some(ref val) = args.window_size {
        builder = builder.window_size(parse_xy(val)?);
    }

    if let Some(size) = args.icon_size {
        builder = builder.icon_size(size);
    }

    if let Some(size) = args.text_size {
        builder = builder.text_size(size);
    }

    for spec in &args.icons {
        let parts: Vec<&str> = spec.split(',').collect();
        if parts.len() != 3 {
            return Err(anyhow::anyhow!(
                "--icon format: 'name,x,y', got '{}'", spec
            ));
        }
        let name = parts[0].trim().to_string();
        let x: i32 = parts[1].trim().parse()
            .map_err(|_| anyhow::anyhow!("invalid x value: '{}'", parts[1].trim()))?;
        let y: i32 = parts[2].trim().parse()
            .map_err(|_| anyhow::anyhow!("invalid y value: '{}'", parts[2].trim()))?;
        builder = builder.icon_position(name, x, y);
    }

    for name in &args.hide_extensions {
        builder = builder.hide_extension(name.clone());
    }

    if let Some(ref path) = args.eula {
        if !path.exists() {
            return Err(anyhow::anyhow!("EULA file not found: {}", path.display()));
        }
        builder = builder.eula(path.clone());
    }

    for spec in &args.add_files {
        // Format: "source_path,target_path" or "source_path,target_path,x,y"
        let parts: Vec<&str> = spec.splitn(4, ',').collect();
        let (src, target, pos) = match parts.len() {
            2 => (PathBuf::from(parts[0].trim()), parts[1].trim().to_string(), None),
            4 => {
                let x: i32 = parts[2].trim().parse()
                    .map_err(|_| anyhow::anyhow!("invalid x value in --add-file: '{}'", parts[2].trim()))?;
                let y: i32 = parts[3].trim().parse()
                    .map_err(|_| anyhow::anyhow!("invalid y value in --add-file: '{}'", parts[3].trim()))?;
                (PathBuf::from(parts[0].trim()), parts[1].trim().to_string(), Some((x, y)))
            }
            _ => return Err(anyhow::anyhow!(
                "--add-file format: 'source_path,target_path' or 'source_path,target_path,x,y', got '{}'", spec
            )),
        };
        if !src.exists() {
            return Err(anyhow::anyhow!("extra file not found: {}", src.display()));
        }
        builder = builder.extra_file(src, target.clone());
        if let Some((x, y)) = pos {
            builder = builder.icon_position(target, x, y);
        }
    }

    Ok(builder.build())
}

fn main() -> Result<()> {
    // Enable info-level logging by default so users see progress and results
    // without setting RUST_LOG. The environment variable still overrides this
    // (e.g. RUST_LOG=debug for more verbose output).
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .init();

    let args = Args::parse();

    log::info!("reading ZIP file: {}", args.input.display());
    let zip_file = BufReader::new(
        File::open(&args.input)
            .with_context(|| format!("failed to open input ZIP: {}", args.input.display()))?,
    );

    log::info!("parsing ZIP entries");
    let mut entries = ZipEntries::from_zip_reader(zip_file)
        .context("failed to parse ZIP entries")?;

    log::info!(
        "found bundle '{}' with {} entries",
        entries.root_name,
        entries.items.len()
    );

    let decorations = build_decorations(&args)
        .context("invalid decoration options")?;

    let fs_lower = args.filesystem.to_ascii_lowercase();
    let (fs_label, fs_path) = match fs_lower.as_str() {
        "hfs+" | "hfs" => {
            log::info!("building HFS+ disk image (file-backed, no size limit)");
            let path = build_hfs_plus_image_to_file(&args.volume_name, &mut entries, &decorations)
                .context("failed to build HFS+ image")?;
            ("HFS+", path)
        }
        "apfs" => {
            log::info!("building APFS disk image (file-backed, no size limit)");
            let path = zip2dmg::apfs::build_apfs_image_to_file(&args.volume_name, &mut entries, &decorations)
                .context("failed to build APFS image")?;
            ("APFS", path)
        }
        other => return Err(anyhow::anyhow!(
            "unknown filesystem '{}'; supported: HFS+, APFS", other
        )),
    };

    let fs_size = std::fs::metadata(&fs_path)
        .map(|m| m.len())
        .unwrap_or(0);
    log::info!("{} image size: {} bytes", fs_label, fs_size);

    log::info!("building GPT partition layout for DMG ({} size: {} bytes)", fs_label, fs_size);
    let layout = match fs_lower.as_str() {
        "apfs" => build_gpt_layout_with_fs(fs_size, FilesystemKind::Apfs),
        _ => build_gpt_layout(fs_size),
    }
    .context("failed to build GPT layout")?;
    log::info!(
        "GPT layout: {} total sectors, HFS+ at LBA {} ({} sectors)",
        layout.total_sectors, layout.hfs_start_lba, layout.hfs_sector_count
    );

    let compression = parse_compression(&args.format)
        .context("invalid format")?;
    log::info!("wrapping in DMG container (format: {}, 8-partition GPT layout)", args.format);
    let output_file = File::create(&args.output)
        .with_context(|| format!("failed to create output DMG: {}", args.output.display()))?;
    let mut writer = BufWriter::new(output_file);

    {
        let mut dmg = udif::DmgWriter::new(&mut writer).compression(compression).zero_fill(false).sector_count_zero(false);

        // Partitions must be added in the order they appear on disk.
        // macOS hdiutil produces this order:
        //   [0] MBR, [1] Primary GPT Header, [2] Primary GPT Entries,
        //   [3] Apple_Free gap, [4] Apple_HFS, [5] Apple_Free tail,
        //   [6] Backup GPT Entries, [7] Backup GPT Header

        // Even small partitions (MBR, GPT, Backup GPT) are Zlib-compressed
        // by macOS hdiutil, so we use add_partition_reader_with_id for them.
        let pre_hfs_data_partitions: [(&str, &[u8], i32); 3] = [
            (&layout.name_mbr, &layout.mbr, -1),
            (&layout.name_primary_gpt_header, &layout.primary_gpt_header, 0),
            (&layout.name_primary_gpt_entries, &layout.primary_gpt_entries, 1),
        ];

        add_data_partitions(&mut dmg, &pre_hfs_data_partitions)?;

        // Apple_Free gap uses Ignore block type (no data in data fork).
        dmg.add_partition_ignore_with_id(&layout.name_free_gap, layout.free_gap.len() as u64 / 512, 2)
            .context("DmgWriter add_partition_ignore free_gap failed")?;

        // HFS+ partition is large and benefits from compression.
        let fs_file = File::open(&fs_path)
            .with_context(|| format!("failed to open {} image: {}", fs_label, fs_path.display()))?;
        let fs_len = fs_file.metadata()
            .with_context(|| format!("failed to stat {} image: {}", fs_label, fs_path.display()))?
            .len();
        let mut fs_reader = BufReader::new(fs_file);
        dmg.add_partition_reader_with_id(&layout.name_hfs, &mut fs_reader, fs_len, 3)
            .context("DmgWriter add_partition_reader filesystem partition failed")?;

        // Apple_Free tail uses Ignore block type.
        dmg.add_partition_ignore_with_id(&layout.name_free_tail, layout.free_tail.len() as u64 / 512, 4)
            .context("DmgWriter add_partition_ignore free_tail failed")?;

        // Backup GPT entries and header are also Zlib-compressed.
        let post_hfs_data_partitions: [(&str, &[u8], i32); 2] = [
            (&layout.name_backup_gpt_entries, &layout.backup_gpt_entries, 5),
            (&layout.name_backup_gpt_header, &layout.backup_gpt_header, 6),
        ];

        add_data_partitions(&mut dmg, &post_hfs_data_partitions)?;

        // Inject EULA license resources (LPic, STR#, TEXT/RTF, TMPL, styl) into
        // the DMG plist's resource-fork, matching create-dmg / hdiutil udifrez.
        if let Some(eula_path) = decorations.eula() {
            log::info!("injecting EULA resources from {}", eula_path.display());
            for (rtype, entry) in build_eula_resources(eula_path)? {
                dmg.add_resource(&rtype, entry);
            }
        }

        dmg.finish()
            .context("DmgWriter finish failed")?;
    }

    writer.flush()?;

    // Clean up the temporary filesystem image file.
    if let Err(e) = std::fs::remove_file(&fs_path) {
        log::warn!("failed to remove temp {} image '{}': {}", fs_label, fs_path.display(), e);
    }

    let output_size = std::fs::metadata(&args.output)
        .map(|m| m.len())
        .unwrap_or(0);
    log::info!("DMG written to {} ({} bytes)", args.output.display(), output_size);

    Ok(())
}

/// Add a batch of in-memory data partitions to the DMG writer.
///
/// Each entry is `(name, data, id)`. The data is wrapped in a `Cursor` and
/// fed to `add_partition_reader_with_id`. Used for the small pre-/post-HFS+
/// partitions (MBR, GPT header/entries, backup GPT) which are all byte slices
/// held in `GptLayout`.
fn add_data_partitions<W: std::io::Write + std::io::Seek>(
    dmg: &mut udif::DmgWriter<W>,
    partitions: &[(&str, &[u8], i32)],
) -> anyhow::Result<()> {
    for (name, data, id) in partitions {
        let mut cursor = std::io::Cursor::new(data.to_vec());
        dmg.add_partition_reader_with_id(name, &mut cursor, data.len() as u64, *id)
            .with_context(|| format!("DmgWriter add_partition_reader '{}' failed", name))?;
    }
    Ok(())
}

/// Build the EULA resource-fork entries for a license file.
///
/// Mirrors create-dmg's `eula-resources-template.xml`: emits the fixed
/// `LPic`, `STR#`, `TMPL`, `styl` resources plus a `TEXT` or `RTF ` resource
/// carrying the license file bytes. Resource type is chosen by file
/// extension: `.rtf` → `RTF `, otherwise `TEXT`.
///
/// Returns `Vec<(resource_type, entry)>` in the order create-dmg emits them
/// (LPic, STR#, TEXT/RTF, TMPL, styl).
fn build_eula_resources(eula_path: &std::path::Path) -> anyhow::Result<Vec<(String, udif::ResourceEntry)>> {
    let license_data = std::fs::read(eula_path)
        .with_context(|| format!("failed to read EULA file '{}': {}", eula_path.display(), eula_path.display()))?;

    let is_rtf = eula_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("rtf"))
        .unwrap_or(false);
    let eula_type = if is_rtf { "RTF " } else { "TEXT" };

    // Fixed resources copied verbatim from create-dmg's eula-resources-template.xml.
    // These are the exact bytes macOS hdiutil udifrez embeds; they must not be
    // regenerated or the license dialog layout will differ from reference DMGs.
    let lpic_data = base64_decode(
        "AAAAAgAAAAAAAAAAAAQAAA==",
    )?;
    let str_data = base64_decode(
        "AAYNRW5nbGlzaCB0ZXN0MQVBZ3JlZQhEaXNhZ3JlZQVQcmludAdTYXZlLi4ueklmIHlvdSBhZ3JlZSB3aXRoIHRoZSB0ZXJtcyBvZiB0aGlzIGxpY2Vuc2UsIGNsaWNrICJBZ3JlZSIgdG8gYWNjZXNzIHRoZSBzb2Z0d2FyZS4gSWYgeW91IGRvIG5vdCBhZ3JlZSwgY2xpY2sgIkRpc2FncmVlIi4=",
    )?;
    let str_data_2 = base64_decode(
        "AAYHRW5nbGlzaAVBZ3JlZQhEaXNhZ3JlZQVQcmludAdTYXZlLi4ue0lmIHlvdSBhZ3JlZSB3aXRoIHRoZSB0ZXJtcyBvZiB0aGlzIGxpY2Vuc2UsIHByZXNzICJBZ3JlZSIgdG8gaW5zdGFsbCB0aGUgc29mdHdhcmUuIElmIHlvdSBkbyBub3QgYWdyZWUsIGNsaWNrICJEaXNhZ3JlZSIu",
    )?;
    let tmpl_data = base64_decode(
        "E0RlZmF1bHQgTGFuZ3VhZ2UgSUREV1JEBUNvdW50T0NOVAQqKioqTFNUQwtzeXMgbGFuZyBJRERXUkQebG9jYWwgcmVzIElEIChvZmZzZXQgZnJvbSA1MDAwRFdSRBAyLWJ5dGUgbGFuZ3VhZ2U/RFdSRAQqKioqTFNURQ==",
    )?;
    let styl_data = base64_decode(
        "AAMAAAAAAAwACQAUAAAAAAAAAAAAAAAAACcADAAJABQBAAAAAAAAAAAAAAAAKgAMAAkAFAAAAAAAAAAAAAA=",
    )?;

    Ok(vec![
        ("LPic".to_string(), udif::ResourceEntry {
            attributes: 0x0000,
            data: lpic_data,
            id: "5000".to_string(),
            name: String::new(),
        }),
        ("STR#".to_string(), udif::ResourceEntry {
            attributes: 0x0000,
            data: str_data,
            id: "5000".to_string(),
            name: "English buttons".to_string(),
        }),
        ("STR#".to_string(), udif::ResourceEntry {
            attributes: 0x0000,
            data: str_data_2,
            id: "5002".to_string(),
            name: "English".to_string(),
        }),
        (eula_type.to_string(), udif::ResourceEntry {
            attributes: 0x0000,
            data: license_data,
            id: "5000".to_string(),
            name: "English".to_string(),
        }),
        ("TMPL".to_string(), udif::ResourceEntry {
            attributes: 0x0000,
            data: tmpl_data,
            id: "128".to_string(),
            name: "LPic".to_string(),
        }),
        ("styl".to_string(), udif::ResourceEntry {
            attributes: 0x0000,
            data: styl_data,
            id: "5000".to_string(),
            name: "English".to_string(),
        }),
    ])
}

/// Decode a base64 string into bytes.
fn base64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}
