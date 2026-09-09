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

//! Build a GPT-wrapped disk image layout for macOS DMG.
//!
//! macOS `hdiutil` / `create-dmg` produce DMG files with 8 partitions:
//!
//! | ID  | Name                                  | Sectors |
//! |-----|---------------------------------------|---------|
//! | -1  | Protective Master Boot Record(MBR:0)  | 1       |
//! | 0   | GPT Header(Primary GPT Header:1)      | 1       |
//! | 1   | GPT Partition Data(Primary GPT Table:2)| 32     |
//! | 2   | (Apple_Free:3)                        | 6       |
//! | 3   | disk image(Apple_HFS:4)               | N       |
//! | 4   | (Apple_Free:5)                        | 2       |
//! | 5   | GPT Partition Data(Backup GPT Table:6)| 32      |
//! | 6   | GPT Header(Backup GPT Header:7)       | 1       |
//!
//! This module uses `fstool`'s `Gpt` and `Mbr` to construct the correct
//! partition table metadata, then provides the raw bytes for each of the
//! 7 non-HFS+ partitions so the caller can feed them to `DmgWriter`.

use fstool::block::BlockDevice;
use fstool::part::{Gpt, Partition, PartitionKind, PartitionTable};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Apple HFS+ GPT type UUID.
const APPLE_HFS_UUID: &str = "48465300-0000-11AA-AA11-00306543ECAC";

/// Apple APFS GPT type UUID.
const APPLE_APFS_UUID: &str = "7C3457EF-0000-11AA-AA11-00306543ECAC";

/// Filesystem kind for GPT partition type/name selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilesystemKind {
    HfsPlus,
    Apfs,
}

impl FilesystemKind {
    /// GPT partition type UUID.
    fn type_uuid(&self) -> &'static str {
        match self {
            FilesystemKind::HfsPlus => APPLE_HFS_UUID,
            FilesystemKind::Apfs => APPLE_APFS_UUID,
        }
    }

    /// GPT partition type label (e.g. `Apple_HFS` / `Apple_APFS`).
    fn type_label(&self) -> &'static str {
        match self {
            FilesystemKind::HfsPlus => "Apple_HFS",
            FilesystemKind::Apfs => "Apple_APFS",
        }
    }
}

const GPT_ENTRIES_SECTORS: u64 = 32;

/// Logical sector size (bytes) — standard for GPT disk images.
const SECTOR_SIZE: u64 = 512;

const FIRST_USABLE_LBA: u64 = 34;

/// Apple_Free gap between GPT entries and HFS+ (6 sectors).
const APPLE_FREE_GAP_SECTORS: u64 = 6;

/// Apple_Free gap between HFS+ and backup GPT (2 sectors).
const APPLE_FREE_TAIL_SECTORS: u64 = 2;

/// A sparse in-memory block device backend that only retains written regions.
///
/// Used by `build_gpt_layout`: `Gpt::write` only touches a few fixed regions
/// (MBR, primary/backup GPT headers and entries — roughly 34 KiB total), but
/// it calls `dev.total_size()` to compute the backup-header LBA. Using
/// `MemoryBackend::new(total)` would pre-allocate a `Vec<u8>` the size of the
/// whole disk (500 MiB for a 500 MiB image), wasting memory and triggering
/// `MemoryBackend`'s 256 MiB soft-cap warning.
///
/// This backend instead keeps a `BTreeMap<offset, bytes>` of only the regions
/// actually written. Unwritten areas read as zero per the `BlockDevice`
/// contract, while `total_size` still reports the full disk size so that
/// `Gpt::write`'s geometry calculation (backup header LBA = total_lba - 1)
/// stays correct. Peak memory usage drops from "whole disk" to "bytes
/// actually written".
struct SparseGptBackend {
    /// Total device capacity in bytes; used only for geometry, never allocated.
    total_size: u64,
    /// Written regions: `offset -> bytes`. Keys are write start offsets;
    /// regions may overlap — reads merge them in order, later writes win.
    ///
    /// `Gpt::write` only ever calls `write_at`, so supporting `write_at` and
    /// `read_at` suffices (reads are performed by `build_gpt_layout` after
    /// writing to extract each partition's bytes).
    writes: BTreeMap<u64, Vec<u8>>,
    /// Implicit cursor required by the `Read/Write/Seek` traits; this backend
    /// does not use it for real I/O.
    cursor: u64,
}

impl SparseGptBackend {
    fn new(total_size: u64) -> Self {
        Self {
            total_size,
            writes: BTreeMap::new(),
            cursor: 0,
        }
    }
}

impl std::io::Read for SparseGptBackend {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        // `Gpt::write` never uses `Read`; this is only a reasonable
        // implementation to satisfy the trait bound.
        if self.cursor >= self.total_size {
            return Ok(0);
        }
        let remaining = (self.total_size - self.cursor) as usize;
        let n = remaining.min(out.len());
        self.read_at(self.cursor, &mut out[..n])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        self.cursor += n as u64;
        Ok(n)
    }
}

impl std::io::Write for SparseGptBackend {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        // `Gpt::write` never uses `Write`; this is only a reasonable
        // implementation to satisfy the trait bound.
        if self.cursor >= self.total_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "write past end of SparseGptBackend",
            ));
        }
        let remaining = (self.total_size - self.cursor) as usize;
        let n = remaining.min(data.len());
        self.write_at(self.cursor, &data[..n])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        self.cursor += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for SparseGptBackend {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let new = match pos {
            std::io::SeekFrom::Start(n) => n as i128,
            std::io::SeekFrom::End(d) => self.total_size as i128 + d as i128,
            std::io::SeekFrom::Current(d) => self.cursor as i128 + d as i128,
        };
        if new < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.cursor = new as u64;
        Ok(self.cursor)
    }
}

impl fstool::block::BlockDevice for SparseGptBackend {
    fn block_size(&self) -> u32 {
        SECTOR_SIZE as u32
    }

    fn total_size(&self) -> u64 {
        self.total_size
    }

    fn sync(&mut self) -> fstool::Result<()> {
        Ok(())
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> fstool::Result<()> {
        let size = self.total_size;
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(fstool::Error::OutOfBounds {
                offset,
                len: buf.len() as u64,
                size,
            })?;
        if end > size {
            return Err(fstool::Error::OutOfBounds {
                offset,
                len: buf.len() as u64,
                size,
            });
        }
        // Zero-fill by default, then overlay the written regions.
        buf.fill(0);
        // `BTreeMap` iterates keys in ascending order; intersect each
        // written region with the target range and overlay it.
        for (&start, data) in self.writes.range(..=offset + buf.len() as u64) {
            let seg_end = start + data.len() as u64;
            if seg_end <= offset {
                continue;
            }
            if start >= offset + buf.len() as u64 {
                break;
            }
            // Intersection: [max(start, offset), min(seg_end, end))
            let overlap_start = start.max(offset);
            let overlap_end = seg_end.min(end);
            let dst_off = (overlap_start - offset) as usize;
            let src_off = (overlap_start - start) as usize;
            let len = (overlap_end - overlap_start) as usize;
            buf[dst_off..dst_off + len].copy_from_slice(&data[src_off..src_off + len]);
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> fstool::Result<()> {
        let size = self.total_size;
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(fstool::Error::OutOfBounds {
                offset,
                len: buf.len() as u64,
                size,
            })?;
        if end > size {
            return Err(fstool::Error::OutOfBounds {
                offset,
                len: buf.len() as u64,
                size,
            });
        }
        if buf.is_empty() {
            return Ok(());
        }
        // The regions written by `Gpt::write` are disjoint and independent,
        // so storing them keyed by offset is sufficient.
        self.writes.insert(offset, buf.to_vec());
        Ok(())
    }
}

fn dmg_partition_name(ptype: &str, pname: &str, id: i32) -> String {
    format!("{ptype}({pname}:{id})")
}

/// The raw bytes and sector layout for each non-HFS+ partition in the DMG.
#[derive(Debug)]
pub struct GptLayout {
    /// MBR sector (512 bytes)
    pub mbr: Vec<u8>,
    /// Primary GPT header sector (512 bytes)
    pub primary_gpt_header: Vec<u8>,
    /// Primary GPT entries (32 sectors = 16384 bytes)
    pub primary_gpt_entries: Vec<u8>,
    /// Apple_Free gap between GPT and HFS+ (6 sectors = 3072 bytes of zeros)
    pub free_gap: Vec<u8>,
    /// Apple_Free gap between HFS+ and backup GPT (2 sectors = 1024 bytes of zeros)
    pub free_tail: Vec<u8>,
    /// Backup GPT entries (32 sectors = 16384 bytes)
    pub backup_gpt_entries: Vec<u8>,
    /// Backup GPT header sector (512 bytes)
    pub backup_gpt_header: Vec<u8>,

    /// LBA where the HFS+ partition starts (should be 40)
    pub hfs_start_lba: u64,
    /// Number of sectors for the HFS+ partition
    pub hfs_sector_count: u64,
    /// Total number of sectors in the disk image
    pub total_sectors: u64,

    /// Partition names for the DMG plist
    pub name_mbr: String,
    pub name_primary_gpt_header: String,
    pub name_primary_gpt_entries: String,
    pub name_free_gap: String,
    pub name_hfs: String,
    pub name_free_tail: String,
    pub name_backup_gpt_entries: String,
    pub name_backup_gpt_header: String,
}

/// Build a GPT layout for a DMG containing an HFS+ partition of the given size.
///
/// `hfs_size_bytes` is the size of the HFS+ image in bytes. The function
/// computes the total disk size, builds the GPT partition table using
/// `fstool`, and extracts the raw bytes for each partition region.
pub fn build_gpt_layout(fs_size_bytes: u64) -> anyhow::Result<GptLayout> {
    build_gpt_layout_with_fs(fs_size_bytes, FilesystemKind::HfsPlus)
}

/// Build the GPT layout with an explicit filesystem kind.
///
/// `fs_kind` selects the GPT partition type UUID and label
/// (`Apple_HFS` vs `Apple_APFS`) for the data partition.
pub fn build_gpt_layout_with_fs(fs_size_bytes: u64, fs_kind: FilesystemKind) -> anyhow::Result<GptLayout> {
    let fs_sector_count = (fs_size_bytes + SECTOR_SIZE - 1) / SECTOR_SIZE;

    // Filesystem partition starts at LBA 40 (FIRST_USABLE_LBA + APPLE_FREE_GAP_SECTORS)
    let hfs_start_lba = FIRST_USABLE_LBA + APPLE_FREE_GAP_SECTORS;

    // Total sectors = FS end + free tail + backup entries + backup header
    let total_sectors = hfs_start_lba + fs_sector_count
        + APPLE_FREE_TAIL_SECTORS + GPT_ENTRIES_SECTORS + 1;

    // Build the GPT partition table with a single Apple filesystem entry
    let apple_fs_type = Uuid::parse_str(fs_kind.type_uuid())
        .map_err(|e| anyhow::anyhow!("invalid Apple filesystem UUID: {e}"))?;

    let partitions = vec![Partition {
        start_lba: hfs_start_lba,
        size_lba: fs_sector_count,
        kind: PartitionKind::Gpt(apple_fs_type),
        uuid: None, // Gpt::build will auto-generate
        name: Some("disk image".to_string()),
        bootable: false,
        attributes: 0,
    }];

    let gpt = Gpt::build(partitions)
        .map_err(|e| anyhow::anyhow!("failed to build GPT: {e}"))?;

    // See the SparseGptBackend docs for why a sparse backend is used here.
    let mut dev = SparseGptBackend::new(total_sectors * SECTOR_SIZE);
    gpt.write(&mut dev)
        .map_err(|e| anyhow::anyhow!("failed to write GPT: {e}"))?;

    // Extract each partition region from the backend
    let mbr = read_lba_range(&dev, 0, 1, SECTOR_SIZE)?;
    let primary_gpt_header = read_lba_range(&dev, 1, 1, SECTOR_SIZE)?;
    let primary_gpt_entries = read_lba_range(&dev, 2, GPT_ENTRIES_SECTORS, SECTOR_SIZE)?;
    let free_gap = vec![0u8; (APPLE_FREE_GAP_SECTORS * SECTOR_SIZE) as usize];

    let hfs_end_lba = hfs_start_lba + fs_sector_count;
    let free_tail = vec![0u8; (APPLE_FREE_TAIL_SECTORS * SECTOR_SIZE) as usize];

    let backup_entries_start = hfs_end_lba + APPLE_FREE_TAIL_SECTORS;
    let backup_gpt_entries = read_lba_range(&dev, backup_entries_start, GPT_ENTRIES_SECTORS, SECTOR_SIZE)?;

    let backup_header_lba = total_sectors - 1;
    let backup_gpt_header = read_lba_range(&dev, backup_header_lba, 1, SECTOR_SIZE)?;

    // DMG partition names (matching macOS convention)
    let name_mbr = dmg_partition_name("Protective Master Boot Record", "MBR", 0);
    let name_primary_gpt_header = dmg_partition_name("GPT Header", "Primary GPT Header", 1);
    let name_primary_gpt_entries = dmg_partition_name("GPT Partition Data", "Primary GPT Table", 2);
    let name_free_gap = dmg_partition_name("", "Apple_Free", 3);
    let name_hfs = dmg_partition_name("disk image", fs_kind.type_label(), 4);
    let name_free_tail = dmg_partition_name("", "Apple_Free", 5);
    let name_backup_gpt_entries = dmg_partition_name("GPT Partition Data", "Backup GPT Table", 6);
    let name_backup_gpt_header = dmg_partition_name("GPT Header", "Backup GPT Header", 7);

    Ok(GptLayout {
        mbr,
        primary_gpt_header,
        primary_gpt_entries,
        free_gap,
        free_tail,
        backup_gpt_entries,
        backup_gpt_header,
        hfs_start_lba,
        hfs_sector_count: fs_sector_count,
        total_sectors,
        name_mbr,
        name_primary_gpt_header,
        name_primary_gpt_entries,
        name_free_gap,
        name_hfs,
        name_free_tail,
        name_backup_gpt_entries,
        name_backup_gpt_header,
    })
}

fn read_lba_range(
    dev: &SparseGptBackend,
    start_lba: u64,
    num_lbas: u64,
    sector_size: u64,
) -> anyhow::Result<Vec<u8>> {
    // GPT sector counts are bounded; use checked_mul + try_into to guard
    // against overflow and platform truncation.
    let offset = start_lba
        .checked_mul(sector_size)
        .ok_or_else(|| anyhow::anyhow!("lba offset overflow"))?;
    let len: usize = (num_lbas
        .checked_mul(sector_size)
        .ok_or_else(|| anyhow::anyhow!("lba length overflow"))?)
        .try_into()
        .map_err(|_| anyhow::anyhow!("lba length exceeds usize"))?;
    let end = offset + len as u64;
    debug_assert!(end <= dev.total_size, "read_lba_range: out of bounds");

    // Zero-fill by default, then overlay the written regions. `writes` is
    // iterated in ascending offset order; each region is intersected with
    // the target range [offset, end) and overlaid.
    let mut out = vec![0u8; len];
    for (&seg_start, data) in dev.writes.range(..=end) {
        let seg_end = seg_start + data.len() as u64;
        if seg_end <= offset {
            continue;
        }
        if seg_start >= end {
            break;
        }
        let overlap_start = seg_start.max(offset);
        let overlap_end = seg_end.min(end);
        let dst_off = (overlap_start - offset) as usize;
        let src_off = (overlap_start - seg_start) as usize;
        let n = (overlap_end - overlap_start) as usize;
        out[dst_off..dst_off + n].copy_from_slice(&data[src_off..src_off + n]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpt_layout_sizes() {
        // 100 MB HFS+ image
        let hfs_size = 100 * 1024 * 1024u64;
        let layout = build_gpt_layout(hfs_size).unwrap();

        assert_eq!(layout.mbr.len(), 512);
        assert_eq!(layout.primary_gpt_header.len(), 512);
        assert_eq!(layout.primary_gpt_entries.len(), 16384);
        assert_eq!(layout.free_gap.len(), 3072);
        assert_eq!(layout.free_tail.len(), 1024);
        assert_eq!(layout.backup_gpt_entries.len(), 16384);
        assert_eq!(layout.backup_gpt_header.len(), 512);

        assert_eq!(layout.hfs_start_lba, 40);
        // 100 MB = 204800 sectors (at 512 bytes/sector)
        assert_eq!(layout.hfs_sector_count, 204800);
    }

    #[test]
    fn test_gpt_layout_names() {
        let layout = build_gpt_layout(1024 * 1024).unwrap();
        assert_eq!(layout.name_mbr, "Protective Master Boot Record(MBR:0)");
        assert_eq!(layout.name_primary_gpt_header, "GPT Header(Primary GPT Header:1)");
        assert_eq!(layout.name_primary_gpt_entries, "GPT Partition Data(Primary GPT Table:2)");
        assert_eq!(layout.name_free_gap, "(Apple_Free:3)");
        assert_eq!(layout.name_hfs, "disk image(Apple_HFS:4)");
        assert_eq!(layout.name_free_tail, "(Apple_Free:5)");
        assert_eq!(layout.name_backup_gpt_entries, "GPT Partition Data(Backup GPT Table:6)");
        assert_eq!(layout.name_backup_gpt_header, "GPT Header(Backup GPT Header:7)");
    }

    #[test]
    fn test_gpt_mbr_signature() {
        let layout = build_gpt_layout(1024 * 1024).unwrap();
        // MBR must end with 0x55AA
        assert_eq!(layout.mbr[510], 0x55);
        assert_eq!(layout.mbr[511], 0xAA);
        // MBR partition type 0xEE (protective)
        assert_eq!(layout.mbr[446 + 4], 0xEE);
    }

    #[test]
    fn test_gpt_header_signature() {
        let layout = build_gpt_layout(1024 * 1024).unwrap();
        // Primary GPT header must start with "EFI PART"
        assert_eq!(&layout.primary_gpt_header[0..8], b"EFI PART");
        // Backup GPT header must also start with "EFI PART"
        assert_eq!(&layout.backup_gpt_header[0..8], b"EFI PART");
    }
}
