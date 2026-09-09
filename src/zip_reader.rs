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

//! ZIP archive reader that extracts entries for DMG creation.
//!
//! Unlike `ZipBundle` (which implements the full `Bundle` trait for signing),
//! this module is a lightweight ZIP reader that only extracts file entries,
//! symlinks, and directories needed for HFS+ image construction.
//!
//! # Streaming design
//!
//! `ZipEntry::File` does **not** hold file content in memory. Instead it stores
//! the ZIP entry index and the uncompressed size. When the HFS+ builder needs
//! to write a file, it calls [`ZipEntries::read_file`] which re-opens the ZIP
//! entry and returns a streaming reader — the file data flows directly from the
//! ZIP through the HFS+ writer's 64 KiB buffer into the block device, without
//! ever being fully buffered in RAM.

use {
    anyhow::{anyhow, Context, Result},
    std::{
        io::{Cursor, Read, Seek},
        path::PathBuf,
    },
};

/// A single entry extracted from a ZIP archive.
#[derive(Clone, Debug)]
pub enum ZipEntry {
    /// A regular file (content streamed on demand, not held in memory).
    File {
        /// Path relative to the archive root (forward-slash separated).
        relative_path: PathBuf,
        /// Index within the ZIP archive (for on-demand reading).
        zip_index: usize,
        /// Uncompressed file size in bytes.
        size: u64,
        /// Unix file mode / permissions.
        mode: u16,
    },
    /// A symbolic link.
    Symlink {
        /// Path relative to the archive root.
        relative_path: PathBuf,
        /// Symlink target path.
        target: PathBuf,
        /// Unix symlink mode.
        mode: u16,
    },
    /// A directory.
    Directory {
        /// Path relative to the archive root.
        relative_path: PathBuf,
        /// Unix directory mode.
        mode: u16,
    },
}

impl ZipEntry {
    /// The relative path of this entry.
    pub fn relative_path(&self) -> &PathBuf {
        match self {
            ZipEntry::File { relative_path, .. } => relative_path,
            ZipEntry::Symlink { relative_path, .. } => relative_path,
            ZipEntry::Directory { relative_path, .. } => relative_path,
        }
    }

}

/// A collection of entries parsed from a ZIP archive.
///
/// Holds a `ZipArchive` internally so that file data can be streamed on demand
/// via [`Self::read_file`], without buffering all files in memory at once.
pub struct ZipEntries<R: Read + Seek> {
    /// The root directory name (e.g. "MyApp.app").
    pub root_name: String,
    /// All entries within the root, in ZIP order.
    pub items: Vec<ZipEntry>,
    /// The underlying ZIP archive, kept alive for streaming reads.
    archive: zip::ZipArchive<R>,
}

impl<R: Read + Seek> ZipEntries<R> {
    pub fn read_file(&mut self, zip_index: usize) -> Result<zip::read::ZipFile<'_, R>> {
        self.archive.by_index(zip_index)
            .with_context(|| format!("failed to open ZIP entry at index {}", zip_index))
    }

    /// Find the `.app` root directory name inside the ZIP.
    ///
    /// Strategy: iterate over all entries, collect first-level path
    /// components ending in `.app`, and pick the **shortest** one; ties are
    /// broken by lexicographic order (smallest wins), keeping the result
    /// deterministic (independent of HashSet's random iteration order).
    fn find_app_root(reader: &mut zip::ZipArchive<R>) -> Result<String> {
        let mut best: Option<String> = None;
        for i in 0..reader.len() {
            let file = reader.by_index(i)?;
            let name = file.name();
            if let Some(first) = name.split('/').next() {
                if !first.ends_with(".app") {
                    continue;
                }
                let candidate = first.to_string();
                let is_better = match &best {
                    None => true,
                    Some(cur) => {
                        // Compare length first (shorter wins), then
                        // lexicographic order (smaller wins).
                        (candidate.len(), &candidate) < (cur.len(), cur)
                    }
                };
                if is_better {
                    best = Some(candidate);
                }
            }
        }

        best.ok_or_else(|| anyhow!("no .app directory found in ZIP archive"))
    }
}

impl ZipEntries<Cursor<Vec<u8>>> {
    pub fn from_zip_data(data: &[u8]) -> Result<Self> {
        Self::from_zip_reader(Cursor::new(data.to_vec()))
    }
}

impl<R: Read + Seek> ZipEntries<R> {
    pub fn from_zip_reader(reader: R) -> Result<Self> {
        let mut archive = zip::ZipArchive::new(reader)
            .context("failed to parse ZIP archive")?;

        let root_name = Self::find_app_root(&mut archive)?;

        log::info!("ZipEntries: parsing root '{}'", root_name);

        let root_prefix = format!("{}/", root_name);
        let mut items = Vec::new();

        for i in 0..archive.len() {
            let file = archive.by_index(i)?;
            let name = file.name().to_string();

            if !name.starts_with(&root_prefix) && name != root_name {
                continue;
            }

            if name == root_name || name == format!("{}/", root_name) {
                continue;
            }

            let rel_path_str = if name.starts_with(&root_prefix) {
                &name[root_prefix.len()..]
            } else {
                continue;
            };

            if rel_path_str.is_empty() {
                continue;
            }

            let relative_path = PathBuf::from(rel_path_str.trim_end_matches('/'));

            if file.is_dir() {
                let mode = file.unix_mode().unwrap_or(0o755) as u16;
                items.push(ZipEntry::Directory { relative_path, mode });
                continue;
            }

            if file.is_symlink() {
                let mode = file.unix_mode().unwrap_or(0o777) as u16;
                drop(file);
                let mut f = archive.by_index(i)?;
                let mut target_str = String::new();
                f.read_to_string(&mut target_str)
                    .context("reading symlink target from ZIP entry")?;
                let target = PathBuf::from(target_str);
                items.push(ZipEntry::Symlink { relative_path, target, mode });
                continue;
            }

            let mode = file.unix_mode().unwrap_or(0o644) as u16;
            let size = file.size();
            items.push(ZipEntry::File {
                relative_path,
                zip_index: i,
                size,
                mode,
            });
        }

        if items.is_empty() {
            return Err(anyhow!(
                "no entries found under root '{}' in ZIP archive",
                root_name
            ));
        }

        Ok(ZipEntries { root_name, items, archive })
    }
}
