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

//! `.DS_Store` binary format generator for macOS Finder window layout.
//!
//! The `.DS_Store` file uses Apple's "Bud1" container format with a
//! buddy allocator and B-tree structure. This implementation generates
//! a minimal valid .DS_Store that matches Finder output.
//!
//! Format overview (from ds_store crate and https://0day.work):
//! - Offset 0: u32 BE = 0x00000001 (fixed magic)
//! - Block 0 (Prelude): "Bud1" + info_offset + info_size + info_offset
//! - Data blocks: B-tree nodes stored in buddy-allocated blocks
//! - Info block: offsets array + DSDB TOC + free list
//!
//! Each B-tree leaf record:
//!   name_len(u32) + name(UTF-16BE) + type_code(4B) + sub_type(4B) + data

use std::io::Cursor;

use crate::DmgDecorations;

const DEFAULT_WINDOW_POS: (i32, i32) = (100, 100);
const DEFAULT_WINDOW_SIZE: (i32, i32) = (640, 480);
const DEFAULT_ICON_SIZE: u32 = 100;
const DEFAULT_TEXT_SIZE: u32 = 16;

const DEFAULT_APP_ICON_POS: (i32, i32) = (150, 190);

const ILOC_POSITIONED_FLAGS: u64 = 0xFFFFFFFFFFFF0000;
const BUD1_OFFSET_COUNT: u32 = 256;
const BUD1_FREE_LIST_LEVELS: u32 = 32;

/// Builder for generating `.DS_Store` binary data.
pub struct DsStoreBuilder {
    /// Window position (x, y).
    window_pos: Option<(i32, i32)>,
    /// Window size (width, height).
    window_size: Option<(i32, i32)>,
    /// Icon size in pixels.
    icon_size: Option<u32>,
    /// Text size in points (icon label font size).
    text_size: Option<u32>,
    /// Icon positions: (name, x, y).
    icon_positions: Vec<(String, i32, i32)>,
    /// File names whose extension should be hidden.
    hide_extensions: Vec<String>,
    /// Background image path (relative to volume root).
    background_path: Option<String>,
}

impl DsStoreBuilder {
    /// Create a new builder with default settings.
    pub fn new() -> Self {
        Self {
            window_pos: None,
            window_size: None,
            icon_size: None,
            text_size: None,
            icon_positions: Vec::new(),
            hide_extensions: Vec::new(),
            background_path: None,
        }
    }

    /// Apply settings from DmgDecorations.
    pub fn apply_decorations(mut self, decorations: &DmgDecorations, app_name: &str) -> Self {
        self.window_pos = decorations.window_pos();
        self.window_size = decorations.window_size();
        self.icon_size = decorations.icon_size();
        self.text_size = decorations.text_size();
        self.icon_positions = decorations.icon_positions().to_vec();
        self.hide_extensions = decorations.hide_extensions().to_vec();

        if self.window_size.is_none() {
            self.window_size = Some(DEFAULT_WINDOW_SIZE);
        }
        if self.icon_size.is_none() {
            self.icon_size = Some(DEFAULT_ICON_SIZE);
        }
        if self.window_pos.is_none() {
            self.window_pos = Some(DEFAULT_WINDOW_POS);
        }

        if let Some((_, filename)) = decorations.background() {
            self.background_path = Some(format!(".background:{}", filename));
        }

        // app_name already includes .app suffix
        let has_app_position = self.icon_positions.iter().any(|(name, _, _)| name.ends_with(".app"));
        if !has_app_position {
            self.icon_positions.push((app_name.to_string(), DEFAULT_APP_ICON_POS.0, DEFAULT_APP_ICON_POS.1));
        }

        if let Some((x, y)) = decorations.app_drop_link() {
            let has_applications = self.icon_positions.iter().any(|(name, _, _)| name == "Applications");
            if !has_applications {
                self.icon_positions.push(("Applications".to_string(), x, y));
            }
        }

        if let Some((x, y)) = decorations.ql_drop_link() {
            let has_quicklook = self.icon_positions.iter().any(|(name, _, _)| name == "QuickLook");
            if !has_quicklook {
                self.icon_positions.push(("QuickLook".to_string(), x, y));
            }
        }

        self
    }

    /// Build the `.DS_Store` binary data.
    pub fn build(self) -> anyhow::Result<Vec<u8>> {
        let mut records: Vec<Bud1Record> = Vec::new();

        let (pos_x, pos_y) = self.window_pos.unwrap_or(DEFAULT_WINDOW_POS);
        let (size_w, size_h) = self.window_size.unwrap_or(DEFAULT_WINDOW_SIZE);

        let bwsp_blob = build_bwsp_blob(pos_x, pos_y, size_w, size_h)?;
        records.push(Bud1Record {
            name: ".".to_string(),
            type_code: *b"bwsp",
            sub_type: *b"blob",
            data: bwsp_blob,
        });

        let icvp_blob = build_icvp_blob(
            self.icon_size.unwrap_or(DEFAULT_ICON_SIZE),
            self.text_size.unwrap_or(DEFAULT_TEXT_SIZE),
            self.background_path.as_deref(),
        )?;
        records.push(Bud1Record {
            name: ".".to_string(),
            type_code: *b"icvp",
            sub_type: *b"blob",
            data: icvp_blob,
        });

        records.push(Bud1Record {
            name: ".".to_string(),
            type_code: *b"vSrn",
            sub_type: *b"long",
            data: 1i32.to_be_bytes().to_vec(),
        });

        for (name, x, y) in &self.icon_positions {
            let iloc_data = build_iloc_blob(*x, *y);
            records.push(Bud1Record {
                name: name.clone(),
                type_code: *b"Iloc",
                sub_type: *b"blob",
                data: iloc_data,
            });
        }

        // extn records: mark file extensions as hidden.
        // Format: bool(1 byte) — 1 means "hide extension".
        for name in &self.hide_extensions {
            records.push(Bud1Record {
                name: name.clone(),
                type_code: *b"extn",
                sub_type: *b"bool",
                data: vec![1u8],
            });
        }

        serialize_bud1(&records)
    }
}

impl Default for DsStoreBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

struct Bud1Record {
    name: String,
    type_code: [u8; 4],
    sub_type: [u8; 4],
    data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// bplist00 blob builders
// ---------------------------------------------------------------------------

fn build_bwsp_blob(pos_x: i32, pos_y: i32, size_w: i32, size_h: i32) -> anyhow::Result<Vec<u8>> {
    let bounds_str = format!(
        "{{{{{x}, {y}}}, {{{w}, {h}}}}}",
        x = pos_x, y = pos_y, w = size_w, h = size_h
    );

    let dict = plist::Dictionary::from_iter([
        ("ShowStatusBar", plist::Value::Boolean(false)),
        ("ShowToolbar", plist::Value::Boolean(false)),
        ("ShowTabView", plist::Value::Boolean(false)),
        ("ContainerShowSidebar", plist::Value::Boolean(true)),
        ("WindowBounds", plist::Value::String(bounds_str)),
        ("ShowSidebar", plist::Value::Boolean(true)),
    ]);

    plist_to_bplist(&plist::Value::Dictionary(dict))
}

fn build_icvp_blob(
    icon_size: u32,
    text_size: u32,
    background_path: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    let bg_type = if background_path.is_some() { 2 } else { 0 };
    let dict = plist::Dictionary::from_iter([
        ("backgroundColorBlue", plist::Value::Real(1.0)),
        ("showIconPreview", plist::Value::Boolean(true)),
        ("textSize", plist::Value::Real(text_size as f64)),
        ("backgroundColorRed", plist::Value::Real(1.0)),
        ("backgroundType", plist::Value::Integer(bg_type.into())),
        ("backgroundColorGreen", plist::Value::Real(1.0)),
        ("gridOffsetX", plist::Value::Real(0.0)),
        ("gridOffsetY", plist::Value::Real(0.0)),
        ("showItemInfo", plist::Value::Boolean(false)),
        ("viewOptionsVersion", plist::Value::Integer(1.into())),
        ("arrangeBy", plist::Value::String("none".into())),
        ("labelOnBottom", plist::Value::Boolean(true)),
        ("iconSize", plist::Value::Real(icon_size as f64)),
        ("gridSpacing", plist::Value::Real(100.0)),
    ]);

    plist_to_bplist(&plist::Value::Dictionary(dict))
}

/// Iloc blob: 16 bytes — x(i32 BE) + y(i32 BE) + flags(u64 BE).
/// Finder uses flags = 0xFFFFFFFFFFFF0000 for positioned items.
fn build_iloc_blob(x: i32, y: i32) -> Vec<u8> {
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&x.to_be_bytes());
    data.extend_from_slice(&y.to_be_bytes());
    data.extend_from_slice(&ILOC_POSITIONED_FLAGS.to_be_bytes());
    data
}

// ---------------------------------------------------------------------------
// Bud1 serialization
// ---------------------------------------------------------------------------

/// Serialize records into the Bud1 binary format.
///
/// Layout:
/// ```text
/// [0x00000001]                    - Fixed magic (4 bytes)
/// [Block 0 @ offset 0, 32B]      - Prelude: "Bud1" + info_offset + info_size + info_offset
/// [Block 1 @ offset 0x40, 32B]   - DSDB root node pointing to leaf
/// [Block 2 @ offset 0x80, N bytes] - B-tree leaf with all records
/// [Info block @ info_offset, 2KB] - Offsets + TOC + free list
/// ```
///
/// Uses buddy allocator encoding: block_address = byte_offset | size_shift
/// Block content starts at byte_offset + 4 (4-byte header before content).
fn serialize_bud1(records: &[Bud1Record]) -> anyhow::Result<Vec<u8>> {
    // Step 1: Build the B-tree leaf node content
    // Leaf: pair_count(0) + record_count + records...
    let mut leaf_content = Vec::new();
    leaf_content.extend_from_slice(&0u32.to_be_bytes());
    leaf_content.extend_from_slice(&(records.len() as u32).to_be_bytes());

    for rec in records {
        let name_utf16 = encode_utf16be(&rec.name);
        let name_char_count = name_utf16.len() / 2;
        leaf_content.extend_from_slice(&(name_char_count as u32).to_be_bytes());
        leaf_content.extend_from_slice(&name_utf16);

        leaf_content.extend_from_slice(&rec.type_code);
        leaf_content.extend_from_slice(&rec.sub_type);

        if &rec.sub_type == b"blob" {
            leaf_content.extend_from_slice(&(rec.data.len() as u32).to_be_bytes());
            leaf_content.extend_from_slice(&rec.data);
        } else {
            leaf_content.extend_from_slice(&rec.data);
        }
    }

    // Step 2: Build the DSDB root node
    let leaf_block_id: u32 = 2;
    let mut dsdb_content = Vec::new();
    dsdb_content.extend_from_slice(&leaf_block_id.to_be_bytes());
    dsdb_content.extend_from_slice(&0u32.to_be_bytes());
    dsdb_content.extend_from_slice(&(records.len() as u32).to_be_bytes());
    dsdb_content.extend_from_slice(&1u32.to_be_bytes());
    dsdb_content.extend_from_slice(&0x00001000u32.to_be_bytes());

    // Step 3: Allocate blocks
    let prelude_addr: u32 = 0x0000;
    let dsdb_addr: u32 = 0x0040;
    let leaf_addr: u32 = 0x0080;

    let leaf_content_with_header = leaf_content.len() as u32 + 4;
    let leaf_block_size = leaf_content_with_header.max(32).next_power_of_two();
    let leaf_size_shift = leaf_block_size.ilog2();

    let info_addr: u32 = 0x1000;
    let info_block_size: u32 = 2048;

    let prelude_encoded = prelude_addr | 5u32;
    let dsdb_encoded = dsdb_addr | 5u32;
    let leaf_encoded = leaf_addr | leaf_size_shift;

    // Step 4: Build the info block content
    let num_offsets: u32 = 3;
    let mut info_content = Vec::new();

    info_content.extend_from_slice(&num_offsets.to_be_bytes());
    info_content.extend_from_slice(&0u32.to_be_bytes());
    info_content.extend_from_slice(&prelude_encoded.to_be_bytes());
    info_content.extend_from_slice(&dsdb_encoded.to_be_bytes());
    info_content.extend_from_slice(&leaf_encoded.to_be_bytes());

    // Pad offsets to BUD1_OFFSET_COUNT entries
    let remaining_offsets = BUD1_OFFSET_COUNT - num_offsets;
    for _ in 0..remaining_offsets {
        info_content.extend_from_slice(&0u32.to_be_bytes());
    }

    // DSDB TOC entry
    info_content.extend_from_slice(&1u32.to_be_bytes());
    info_content.push(4u8);
    info_content.extend_from_slice(b"DSDB");
    info_content.extend_from_slice(&1u32.to_be_bytes());

    // Free list (BUD1_FREE_LIST_LEVELS levels); level 5 has free blocks in the gaps
    for level in 0..BUD1_FREE_LIST_LEVELS {
        if level == 5 {
            info_content.extend_from_slice(&2u32.to_be_bytes());
            info_content.extend_from_slice(&0x20u32.to_be_bytes());
            info_content.extend_from_slice(&0x60u32.to_be_bytes());
        } else {
            info_content.extend_from_slice(&0u32.to_be_bytes());
        }
    }

    // Step 5: Assemble the complete file
    let total_size = (info_addr + 4 + info_block_size) as usize;
    let mut buf = vec![0u8; total_size];

    buf[0..4].copy_from_slice(&0x00000001u32.to_be_bytes());

    let p = (prelude_addr + 4) as usize;
    buf[p..p + 4].copy_from_slice(b"Bud1");
    buf[p + 4..p + 8].copy_from_slice(&info_addr.to_be_bytes());
    buf[p + 8..p + 12].copy_from_slice(&info_block_size.to_be_bytes());
    buf[p + 12..p + 16].copy_from_slice(&info_addr.to_be_bytes());

    let d = (dsdb_addr + 4) as usize;
    buf[d..d + dsdb_content.len()].copy_from_slice(&dsdb_content);

    let l = (leaf_addr + 4) as usize;
    buf[l..l + leaf_content.len()].copy_from_slice(&leaf_content);

    let i = (info_addr + 4) as usize;
    buf[i..i + info_content.len()].copy_from_slice(&info_content);

    Ok(buf)
}

/// Encode a string as UTF-16BE bytes.
fn encode_utf16be(s: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(s.len() * 2);
    for ch in s.encode_utf16() {
        buf.extend_from_slice(&ch.to_be_bytes());
    }
    buf
}

/// Serialize a plist::Value to bplist00 format.
fn plist_to_bplist(value: &plist::Value) -> anyhow::Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    plist::to_writer_binary(&mut buf, value)?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ds_store_basic() {
        let data = DsStoreBuilder::new()
            .apply_decorations(&DmgDecorations::default(), "MyApp")
            .build()
            .unwrap();

        // First 4 bytes must be 0x00000001 (Bud1 format magic)
        assert_eq!(&data[0..4], &[0, 0, 0, 1]);
        // "Bud1" at offset 4 (prelude block content)
        assert_eq!(&data[4..8], b"Bud1");
    }

    #[test]
    fn test_ds_store_with_applications() {
        let decorations = DmgDecorations::builder()
            .app_drop_link((490, 190))
            .build();

        let data = DsStoreBuilder::new()
            .apply_decorations(&decorations, "TestApp")
            .build()
            .unwrap();

        assert_eq!(&data[0..4], &[0, 0, 0, 1]);
        assert_eq!(&data[4..8], b"Bud1");
    }

    #[test]
    fn test_ds_store_with_background() {
        let decorations = DmgDecorations::builder()
            .background(std::path::PathBuf::from("/tmp/bg.png"), "bg.png".to_string())
            .window_size((800, 600))
            .icon_size(128)
            .build();

        let data = DsStoreBuilder::new()
            .apply_decorations(&decorations, "BgApp")
            .build()
            .unwrap();

        assert_eq!(&data[0..4], &[0, 0, 0, 1]);
        assert_eq!(&data[4..8], b"Bud1");
    }

    /// Comprehensive test: verify the Bud1 structure can be parsed back
    /// using the same logic as the ds_store crate.
    #[test]
    fn test_ds_store_roundtrip_parse() {
        let decorations = DmgDecorations::builder()
            .app_drop_link((450, 190))
            .window_pos((200, 560))
            .window_size((600, 400))
            .build();

        let data = DsStoreBuilder::new()
            .apply_decorations(&decorations, "XClaw DIY.app")
            .build()
            .unwrap();

        // Parse using the same logic as ds_store crate
        // 1. Magic
        assert_eq!(&data[0..4], &[0, 0, 0, 1], "Magic must be 0x00000001");

        // 2. Prelude block (Block 0 at offset 0, size 32)
        // Content at data[4..36]: "Bud1" + info_offset + info_size + info_offset
        assert_eq!(&data[4..8], b"Bud1", "Prelude must start with Bud1");
        let info_offset = u32::from_be_bytes(data[8..12].try_into().unwrap());
        let _info_size = u32::from_be_bytes(data[12..16].try_into().unwrap());
        let info_offset_check = u32::from_be_bytes(data[16..20].try_into().unwrap());
        assert_eq!(info_offset, info_offset_check, "Info offset check must match");

        // 3. Info block content at info_offset + 4
        let info_start = (info_offset + 4) as usize;
        let num_offsets = u32::from_be_bytes(data[info_start..info_start+4].try_into().unwrap());
        assert_eq!(num_offsets, 3, "Must have 3 block offsets");

        // Read offsets
        let mut off_pos = info_start + 8; // skip num_offsets + 4 unknown bytes
        let mut offsets = Vec::new();
        for _ in 0..num_offsets {
            offsets.push(u32::from_be_bytes(data[off_pos..off_pos+4].try_into().unwrap()));
            off_pos += 4;
        }

        // Verify block addresses
        for (i, &raw_off) in offsets.iter().enumerate() {
            let addr = raw_off & !0x1fu32;
            let shift = raw_off & 0x1f;
            let block_size = 1u32 << shift;
            let content_start = (addr + 4) as usize;
            assert!(
                content_start + block_size as usize <= data.len(),
                "Block {} content must fit in buffer (start={}, size={}, buf_len={})",
                i, content_start, block_size, data.len()
            );
        }

        // 4. Parse DSDB block (Block 1)
        let dsdb_raw = offsets[1];
        let dsdb_addr = (dsdb_raw & !0x1f) as usize;
        let dsdb_content_start = dsdb_addr + 4;
        let root_node = u32::from_be_bytes(data[dsdb_content_start..dsdb_content_start+4].try_into().unwrap());
        assert_eq!(root_node, 2, "Root node must be block 2");

        // 5. Parse Leaf block (Block 2) and verify records
        let leaf_raw = offsets[2];
        let leaf_addr = (leaf_raw & !0x1f) as usize;
        let leaf_content_start = leaf_addr + 4;
        let pair_count = u32::from_be_bytes(data[leaf_content_start..leaf_content_start+4].try_into().unwrap());
        assert_eq!(pair_count, 0, "Leaf must have pair_count=0");

        let record_count = u32::from_be_bytes(data[leaf_content_start+4..leaf_content_start+8].try_into().unwrap());
        assert_eq!(record_count, 5, "Must have 5 records (bwsp, icvp, vSrn, 2xIloc)");

        // Parse leaf records
        let mut pos = leaf_content_start + 8;
        let mut parsed_records: Vec<(String, String, String)> = Vec::new();
        for _ in 0..record_count {
            let name_len = u32::from_be_bytes(data[pos..pos+4].try_into().unwrap()) as usize;
            pos += 4;
            let name_bytes = &data[pos..pos+name_len*2];
            let name_u16: Vec<u16> = name_bytes.chunks(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
            let name = String::from_utf16(&name_u16).unwrap();
            pos += name_len * 2;
            let type_code = std::str::from_utf8(&data[pos..pos+4]).unwrap().to_string();
            pos += 4;
            let sub_type = std::str::from_utf8(&data[pos..pos+4]).unwrap()
                .trim_end_matches('\0').to_string();
            pos += 4;

            if sub_type == "blob" {
                let blob_len = u32::from_be_bytes(data[pos..pos+4].try_into().unwrap()) as usize;
                pos += 4 + blob_len;
            } else if sub_type == "long" {
                pos += 4;
            }

            parsed_records.push((name, type_code, sub_type));
        }

        // Verify expected records
        assert_eq!(parsed_records[0], (".".to_string(), "bwsp".to_string(), "blob".to_string()));
        assert_eq!(parsed_records[1], (".".to_string(), "icvp".to_string(), "blob".to_string()));
        assert_eq!(parsed_records[2], (".".to_string(), "vSrn".to_string(), "long".to_string()));
        assert_eq!(parsed_records[3].1, "Iloc");
        assert_eq!(parsed_records[3].2, "blob");
        assert_eq!(parsed_records[4].1, "Iloc");
        assert_eq!(parsed_records[4].2, "blob");

        // Verify Iloc names
        let iloc_names: Vec<&str> = parsed_records[3..5].iter().map(|r| r.0.as_str()).collect();
        assert!(iloc_names.contains(&"XClaw DIY.app"), "Must have XClaw DIY.app Iloc");
        assert!(iloc_names.contains(&"Applications"), "Must have Applications Iloc");
    }

    /// Test that ds_store crate (third-party parser) can parse our output.
    #[test]
    fn test_ds_store_parseable_by_ds_store_crate() {
        let decorations = DmgDecorations::builder()
            .app_drop_link((450, 190))
            .window_pos((200, 560))
            .window_size((600, 400))
            .build();

        let data = DsStoreBuilder::new()
            .apply_decorations(&decorations, "XClaw DIY.app")
            .build()
            .unwrap();

        // Parse with ds_store crate
        let store = ds_store::DsStore::new(&data)
            .expect("ds_store crate should parse our generated .DS_Store");

        let contents = store.contents();

        // Verify "." directory has bwsp, icvp, vSrn
        let dot = contents.get(".").expect("Must have '.' entry");
        assert!(dot.contains_key("bwsp"), "Must have bwsp");
        assert!(dot.contains_key("icvp"), "Must have icvp");
        assert!(dot.contains_key("vSrn"), "Must have vSrn");

        // Verify Iloc entries
        let app = contents.get("XClaw DIY.app").expect("Must have XClaw DIY.app entry");
        assert!(app.contains_key("Iloc"), "Must have Iloc for XClaw DIY.app");

        let apps = contents.get("Applications").expect("Must have Applications entry");
        assert!(apps.contains_key("Iloc"), "Must have Iloc for Applications");
    }
}
