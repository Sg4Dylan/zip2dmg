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

//! Integration tests for zip2dmg.
//!
//! These tests construct minimal .app bundle ZIPs in memory, then verify
//! the complete ZIP → HFS+ → DMG pipeline.

use std::io::{Cursor, Write};

/// Helper: build an HFS+ image to file, read it back, and clean up.
///
/// Uses the file-backed path so the writer streams file content directly
/// to disk instead of holding the whole image in memory.
fn build_hfs_image_to_vec(
    volume_name: &str,
    entries: &mut zip2dmg::ZipEntries<std::io::Cursor<Vec<u8>>>,
    decorations: &zip2dmg::DmgDecorations,
) -> Vec<u8> {
    let path = zip2dmg::build_hfs_plus_image_to_file(volume_name, entries, decorations)
        .expect("failed to build HFS+ image to file");
    let data = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read HFS+ image from '{}': {}", path.display(), e));
    let _ = std::fs::remove_file(&path);
    data
}

/// Helper: create a minimal .app bundle as a ZIP archive in memory.
///
/// The bundle contains:
/// - `TestApp.app/` (directory)
/// - `TestApp.app/Contents/` (directory)
/// - `TestApp.app/Contents/Info.plist` (small XML file)
/// - `TestApp.app/Contents/MacOS/` (directory)
/// - `TestApp.app/Contents/MacOS/TestApp` (small Mach-O stub)
/// - `TestApp.app/Contents/Resources/` (directory)
/// - `TestApp.app/Frameworks/` (directory)
/// - `TestApp.app/Frameworks/Helper.framework/` (directory)
/// - `TestApp.app/Frameworks/Helper.framework/Versions/` (directory)
/// - `TestApp.app/Frameworks/Helper.framework/Versions/A/` (directory)
/// - `TestApp.app/Frameworks/Helper.framework/Versions/A/Helper` (small file)
/// - `TestApp.app/Frameworks/Helper.framework/Versions/Current` (symlink → A)
/// - `TestApp.app/Frameworks/Helper.framework/Helper` (symlink → Versions/Current/Helper)
fn create_minimal_app_zip() -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buf);
        let dir_opts = zip::write::SimpleFileOptions::default()
            .unix_permissions(0o755)
            .compression_method(zip::CompressionMethod::Stored);
        let file_opts = zip::write::SimpleFileOptions::default()
            .unix_permissions(0o644)
            .compression_method(zip::CompressionMethod::Deflated);

        // Directories
        writer.add_directory("TestApp.app/", dir_opts.clone()).unwrap();
        writer.add_directory("TestApp.app/Contents/", dir_opts.clone()).unwrap();
        writer.add_directory("TestApp.app/Contents/MacOS/", dir_opts.clone()).unwrap();
        writer.add_directory("TestApp.app/Contents/Resources/", dir_opts.clone()).unwrap();
        writer.add_directory("TestApp.app/Frameworks/", dir_opts.clone()).unwrap();
        writer.add_directory("TestApp.app/Frameworks/Helper.framework/", dir_opts.clone()).unwrap();
        writer.add_directory("TestApp.app/Frameworks/Helper.framework/Versions/", dir_opts.clone()).unwrap();
        writer.add_directory("TestApp.app/Frameworks/Helper.framework/Versions/A/", dir_opts.clone()).unwrap();

        // Info.plist
        let info_plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>com.example.testapp</string>
    <key>CFBundleName</key>
    <string>TestApp</string>
    <key>CFBundleExecutable</key>
    <string>TestApp</string>
    <key>CFBundleVersion</key>
    <string>1.0</string>
</dict>
</plist>"#;
        writer.start_file("TestApp.app/Contents/Info.plist", file_opts.clone()).unwrap();
        writer.write_all(info_plist.as_bytes()).unwrap();

        // Main executable (tiny stub — not a real Mach-O, just placeholder bytes)
        let exe_data = b"\xfe\xed\xfa\xce\x00\x00\x00\x01"; // Magic + placeholder
        let exe_opts = zip::write::SimpleFileOptions::default()
            .unix_permissions(0o755)
            .compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("TestApp.app/Contents/MacOS/TestApp", exe_opts).unwrap();
        writer.write_all(exe_data).unwrap();

        // Framework helper file
        let helper_data = b"helper-content-placeholder";
        writer.start_file("TestApp.app/Frameworks/Helper.framework/Versions/A/Helper", file_opts.clone()).unwrap();
        writer.write_all(helper_data).unwrap();

        // Symlinks (framework convention)
        writer.add_symlink(
            "TestApp.app/Frameworks/Helper.framework/Versions/Current",
            "A",
            zip::write::SimpleFileOptions::default(),
        ).unwrap();
        writer.add_symlink(
            "TestApp.app/Frameworks/Helper.framework/Helper",
            "Versions/Current/Helper",
            zip::write::SimpleFileOptions::default(),
        ).unwrap();

        writer.finish().unwrap();
    }
    buf.into_inner()
}

#[test]
fn test_zip_entries_parse() {
    let zip_data = create_minimal_app_zip();
    let entries = zip2dmg::ZipEntries::from_zip_data(&zip_data)
        .expect("failed to parse ZIP entries");

    assert_eq!(entries.root_name, "TestApp.app");
    // Should have: Info.plist, TestApp, Helper, Current (symlink), Helper (symlink)
    // + directories that were explicit entries
    assert!(!entries.items.is_empty(), "should have some entries");
}

#[test]
fn test_hfs_plus_image_build() {
    let zip_data = create_minimal_app_zip();
    let mut entries = zip2dmg::ZipEntries::from_zip_data(&zip_data)
        .expect("failed to parse ZIP entries");

    let hfs_image = build_hfs_image_to_vec("TestApp", &mut entries, &zip2dmg::DmgDecorations::default());

    // The image should be at least 16 MiB (our minimum).
    assert!(hfs_image.len() >= 16 * 1024 * 1024, "HFS+ image too small: {} bytes", hfs_image.len());

    // Check HFS+ signature: bytes at offset 1024 should be "H+" (0x482B) for HFS+.
    // The volume header starts at byte 1024.
    assert!(hfs_image.len() > 1026, "image too short for volume header");
    let sig = u16::from_be_bytes([hfs_image[1024], hfs_image[1025]]);
    assert_eq!(sig, 0x482B, "HFS+ signature not found (got 0x{:04X})", sig);
}

#[test]
fn test_zip_entries_with_symlinks() {
    let zip_data = create_minimal_app_zip();
    let entries = zip2dmg::ZipEntries::from_zip_data(&zip_data)
        .expect("failed to parse ZIP entries");

    // Should find symlink entries.
    let symlinks: Vec<_> = entries.items.iter().filter_map(|e| {
        match e {
            zip2dmg::ZipEntry::Symlink { relative_path, target, .. } => Some((relative_path, target)),
            _ => None,
        }
    }).collect();

    assert!(!symlinks.is_empty(), "should have at least one symlink");

    // Verify the Helper symlink exists.
    let helper_sym = symlinks.iter().find(|(p, _)| {
        p.to_string_lossy().contains("Helper.framework/Helper")
    });
    assert!(helper_sym.is_some(), "Helper symlink not found");

    let (_, target) = helper_sym.unwrap();
    assert_eq!(target.to_string_lossy(), "Versions/Current/Helper",
        "Helper symlink target mismatch");
}

#[test]
fn test_estimate_image_size_reasonable() {
    let zip_data = create_minimal_app_zip();
    let mut entries = zip2dmg::ZipEntries::from_zip_data(&zip_data)
        .expect("failed to parse ZIP entries");

    let hfs_image = build_hfs_image_to_vec("TestApp", &mut entries, &zip2dmg::DmgDecorations::default());

    // For a minimal app, the image should be relatively small (16-64 MiB).
    let size_mb = hfs_image.len() as f64 / (1024.0 * 1024.0);
    assert!(size_mb < 100.0, "minimal app image too large: {:.1} MB", size_mb);
    assert!(size_mb >= 16.0, "minimal app image too small: {:.1} MB", size_mb);
}

#[test]
fn test_dmg_container_wrap() {
    let zip_data = create_minimal_app_zip();
    let mut entries = zip2dmg::ZipEntries::from_zip_data(&zip_data)
        .expect("failed to parse ZIP entries");

    let hfs_image = build_hfs_image_to_vec("TestApp", &mut entries, &zip2dmg::DmgDecorations::default());

    // Check how much of the HFS+ image is non-zero.
    let nonzero = hfs_image.iter().filter(|&&b| b != 0).count();
    eprintln!("HFS+ image: {} bytes, non-zero: {} ({:.1}%)",
        hfs_image.len(), nonzero, nonzero as f64 / hfs_image.len() as f64 * 100.0);

    // Wrap in DMG container.
    let mut dmg_buf = Cursor::new(Vec::new());
    {
        let mut dmg = udif::DmgWriter::new(&mut dmg_buf);
        dmg.add_partition("Apple_HFS", &hfs_image)
            .expect("DmgWriter add_partition failed");
        dmg.finish()
            .expect("DmgWriter finish failed");
    }
    let dmg_data = dmg_buf.into_inner();

    eprintln!("DMG size: {} bytes, HFS+ size: {} bytes", dmg_data.len(), hfs_image.len());
    eprintln!("Last 32 bytes: {:02x?}", &dmg_data[dmg_data.len().saturating_sub(32)..]);
    eprintln!("First 32 bytes: {:02x?}", &dmg_data[..32.min(dmg_data.len())]);

    // Search for "koly" in the DMG data.
    let koly_pos = dmg_data.windows(4).position(|w| w == b"koly");
    eprintln!("'koly' found at position: {:?}", koly_pos);

    // DMG should be at least 512 bytes (plist + koly header).
    assert!(dmg_data.len() > 512,
        "DMG too small: {} bytes", dmg_data.len());

    // Check DMG magic: "koly" header is 512 bytes and sits at the very end.
    // The 4-byte magic is at the start of the koly header.
    let koly_offset = dmg_data.len() - 512;
    let magic = &dmg_data[koly_offset..koly_offset + 4];
    assert_eq!(magic, b"koly", "DMG koly magic not found at expected offset");
}

/// Test that duplicate entries in a ZIP are handled gracefully.
#[test]
fn test_duplicate_entries_dedup() {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buf);
        let dir_opts = zip::write::SimpleFileOptions::default()
            .unix_permissions(0o755)
            .compression_method(zip::CompressionMethod::Stored);
        let file_opts = zip::write::SimpleFileOptions::default()
            .unix_permissions(0o644)
            .compression_method(zip::CompressionMethod::Deflated);

        writer.add_directory("TestApp.app/", dir_opts.clone()).unwrap();
        writer.add_directory("TestApp.app/Contents/", dir_opts.clone()).unwrap();
        // Write a file entry that has the same name as a directory component
        // that will be created as an implicit parent. This tests that the
        // HFS+ builder deduplicates correctly.
        writer.start_file("TestApp.app/Contents/Info.plist", file_opts.clone()).unwrap();
        writer.write_all(b"<plist/>").unwrap();
        // Also add a second file to create more directory entries.
        writer.start_file("TestApp.app/Contents/MacOS/TestApp", file_opts.clone()).unwrap();
        writer.write_all(b"stub").unwrap();

        writer.finish().unwrap();
    }
    let zip_data = buf.into_inner();

    let mut entries = zip2dmg::ZipEntries::from_zip_data(&zip_data)
        .expect("failed to parse ZIP entries");

    // Should succeed without errors — implicit parent directories are deduped.
    let hfs_image = build_hfs_image_to_vec("TestApp", &mut entries, &zip2dmg::DmgDecorations::default());

    assert!(hfs_image.len() >= 16 * 1024 * 1024, "HFS+ image too small");
}

/// Test XZ compression in DMG container.
#[test]
fn test_dmg_xz_compression() {
    let zip_data = create_minimal_app_zip();
    let mut entries = zip2dmg::ZipEntries::from_zip_data(&zip_data)
        .expect("failed to parse ZIP entries");

    let hfs_image = build_hfs_image_to_vec("TestApp", &mut entries, &zip2dmg::DmgDecorations::default());

    // Wrap in DMG container using XZ compression.
    let mut dmg_buf = Cursor::new(Vec::new());
    {
        let mut dmg = udif::DmgWriter::new(&mut dmg_buf)
            .compression(udif::CompressionMethod::Xz);
        dmg.add_partition("Apple_HFS", &hfs_image)
            .expect("DmgWriter add_partition failed");
        dmg.finish()
            .expect("DmgWriter finish failed");
    }
    let dmg_data = dmg_buf.into_inner();

    // DMG should be valid with koly header.
    assert!(dmg_data.len() > 512, "XZ DMG too small: {} bytes", dmg_data.len());
    let koly_offset = dmg_data.len() - 512;
    let magic = &dmg_data[koly_offset..koly_offset + 4];
    assert_eq!(magic, b"koly", "DMG koly magic not found");

    // XZ-compressed DMG should be smaller than raw (uncompressed) DMG
    // for our sparse test data.
    let mut raw_buf = Cursor::new(Vec::new());
    {
        let mut dmg = udif::DmgWriter::new(&mut raw_buf)
            .compression(udif::CompressionMethod::Raw);
        dmg.add_partition("Apple_HFS", &hfs_image)
            .expect("DmgWriter add_partition failed");
        dmg.finish()
            .expect("DmgWriter finish failed");
    }
    let raw_data = raw_buf.into_inner();

    assert!(dmg_data.len() < raw_data.len(),
        "XZ DMG ({} bytes) should be smaller than raw DMG ({} bytes)",
        dmg_data.len(), raw_data.len());
}

/// Test that all compression methods produce valid DMGs.
#[test]
fn test_all_compression_methods() {
    let zip_data = create_minimal_app_zip();
    let mut entries = zip2dmg::ZipEntries::from_zip_data(&zip_data)
        .expect("failed to parse ZIP entries");

    let hfs_image = build_hfs_image_to_vec("TestApp", &mut entries, &zip2dmg::DmgDecorations::default());

    let methods = vec![
        ("raw", udif::CompressionMethod::Raw),
        ("zlib", udif::CompressionMethod::Zlib),
        ("bzip2", udif::CompressionMethod::Bzip2),
        ("lzfse", udif::CompressionMethod::Lzfse),
        ("xz", udif::CompressionMethod::Xz),
    ];

    for (name, method) in methods {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut dmg = udif::DmgWriter::new(&mut buf).compression(method);
            dmg.add_partition("Apple_HFS", &hfs_image)
                .unwrap_or_else(|e| panic!("{} add_partition failed: {}", name, e));
            dmg.finish()
                .unwrap_or_else(|e| panic!("{} finish failed: {}", name, e));
        }
        let data = buf.into_inner();

        // Verify koly header.
        assert!(data.len() > 512, "{} DMG too small", name);
        let koly_offset = data.len() - 512;
        assert_eq!(&data[koly_offset..koly_offset + 4], b"koly",
            "{} DMG missing koly magic", name);

        eprintln!("{}: {} bytes", name, data.len());
    }
}

/// Test that an empty .app bundle (just directories, no files) works.
#[test]
fn test_empty_app_bundle() {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buf);
        let dir_opts = zip::write::SimpleFileOptions::default()
            .unix_permissions(0o755)
            .compression_method(zip::CompressionMethod::Stored);

        writer.add_directory("EmptyApp.app/", dir_opts.clone()).unwrap();
        writer.add_directory("EmptyApp.app/Contents/", dir_opts.clone()).unwrap();

        writer.finish().unwrap();
    }
    let zip_data = buf.into_inner();

    let mut entries = zip2dmg::ZipEntries::from_zip_data(&zip_data)
        .expect("failed to parse ZIP entries");

    let hfs_image = build_hfs_image_to_vec("EmptyApp", &mut entries, &zip2dmg::DmgDecorations::default());

    assert!(hfs_image.len() >= 16 * 1024 * 1024, "HFS+ image too small");
}
