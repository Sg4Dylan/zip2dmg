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

//! Convert a ZIP archive containing a macOS .app bundle into an unsigned DMG disk image.
//!
//! This tool reads a ZIP file, extracts its entries, builds an HFS+ filesystem
//! image using `fstool`, wraps it in a DMG container using `udif`, and writes
//! the result to disk. It does NOT perform any code signing — use `rcodesign sign`
//! separately for that.
//!
//! # Pipeline
//!
//! ```text
//! ZIP → parse entries → HFS+ image → DMG container → unsigned .dmg
//! ```
//!
//! # Usage
//!
//! ```bash
//! zip2dmg --input signed.zip --output app.dmg --volume-name MyApp
//! ```

use std::path::PathBuf;

/// Decorations added to a DMG for the macOS drag-install experience.
///
/// This struct captures the options that `create-dmg` provides via CLI
/// flags (`--app-drop-link`, `--background`, etc.) and applies them
/// during HFS+ image construction.
///
/// Construct via [`DmgDecorations::builder()`].
#[derive(Clone, Debug, Default)]
pub struct DmgDecorations {
    /// Create an `/Applications` symlink at the given icon position.
    app_drop_link: Option<(i32, i32)>,

    /// Create a `/QuickLook` symlink at the given icon position.
    ql_drop_link: Option<(i32, i32)>,

    /// Background image to embed at `/.background/<filename>`.
    background: Option<(PathBuf, String)>,

    /// Volume icon (`.icns`) to embed at `/.VolumeIcon.icns`.
    volicon: Option<PathBuf>,

    /// Extra files to add: (host_path, target_path_in_dmg).
    extra_files: Vec<(PathBuf, String)>,

    /// Window position (x, y) for the Finder window.
    window_pos: Option<(i32, i32)>,

    /// Window size (width, height) for the Finder window.
    window_size: Option<(i32, i32)>,

    /// Icon size in pixels.
    icon_size: Option<u32>,

    /// Text size in points (icon label font size).
    text_size: Option<u32>,

    /// Icon positions for specific entries: (name, x, y).
    icon_positions: Vec<(String, i32, i32)>,

    /// File names whose extension should be hidden in Finder.
    hide_extensions: Vec<String>,

    /// Optional EULA file (plain text or RTF) to embed as DMG license resources.
    eula: Option<PathBuf>,
}

impl DmgDecorations {
    /// Create a new builder for configuring DMG decorations.
    pub fn builder() -> DmgDecorationsBuilder {
        DmgDecorationsBuilder::default()
    }

    // --- Read accessors (used by hfs.rs and ds_store.rs) ---

    pub(crate) fn app_drop_link(&self) -> Option<(i32, i32)> {
        self.app_drop_link
    }

    pub(crate) fn ql_drop_link(&self) -> Option<(i32, i32)> {
        self.ql_drop_link
    }

    pub(crate) fn background(&self) -> Option<&(PathBuf, String)> {
        self.background.as_ref()
    }

    pub(crate) fn volicon(&self) -> Option<&PathBuf> {
        self.volicon.as_ref()
    }

    pub(crate) fn extra_files(&self) -> &[(PathBuf, String)] {
        &self.extra_files
    }

    pub(crate) fn window_pos(&self) -> Option<(i32, i32)> {
        self.window_pos
    }

    pub(crate) fn window_size(&self) -> Option<(i32, i32)> {
        self.window_size
    }

    pub(crate) fn icon_size(&self) -> Option<u32> {
        self.icon_size
    }

    pub(crate) fn text_size(&self) -> Option<u32> {
        self.text_size
    }

    pub(crate) fn icon_positions(&self) -> &[(String, i32, i32)] {
        &self.icon_positions
    }

    pub(crate) fn hide_extensions(&self) -> &[String] {
        &self.hide_extensions
    }

    pub fn eula(&self) -> Option<&PathBuf> {
        self.eula.as_ref()
    }

    /// Whether any decoration requires generating a `.DS_Store` file.
    pub(crate) fn needs_ds_store(&self) -> bool {
        self.window_pos.is_some()
            || self.window_size.is_some()
            || self.icon_size.is_some()
            || self.text_size.is_some()
            || !self.icon_positions.is_empty()
            || !self.hide_extensions.is_empty()
            || self.background.is_some()
            || self.app_drop_link.is_some()
            || self.ql_drop_link.is_some()
    }
}

/// Builder for [`DmgDecorations`].
#[derive(Clone, Debug, Default)]
pub struct DmgDecorationsBuilder {
    inner: DmgDecorations,
}

impl DmgDecorationsBuilder {
    /// Create an `/Applications` symlink at the given icon position.
    pub fn app_drop_link(mut self, pos: (i32, i32)) -> Self {
        self.inner.app_drop_link = Some(pos);
        self
    }

    /// Create a `/QuickLook` symlink at the given icon position.
    pub fn ql_drop_link(mut self, pos: (i32, i32)) -> Self {
        self.inner.ql_drop_link = Some(pos);
        self
    }

    /// Embed a background image from `src` as `/.background/<filename>`.
    pub fn background(mut self, src: PathBuf, filename: String) -> Self {
        self.inner.background = Some((src, filename));
        self
    }

    /// Embed a volume icon (`.icns`).
    pub fn volicon(mut self, src: PathBuf) -> Self {
        self.inner.volicon = Some(src);
        self
    }

    /// Add an extra file from `src` to `target_path` in the DMG.
    pub fn extra_file(mut self, src: PathBuf, target: String) -> Self {
        self.inner.extra_files.push((src, target));
        self
    }

    /// Set the Finder window position.
    pub fn window_pos(mut self, pos: (i32, i32)) -> Self {
        self.inner.window_pos = Some(pos);
        self
    }

    /// Set the Finder window size.
    pub fn window_size(mut self, size: (i32, i32)) -> Self {
        self.inner.window_size = Some(size);
        self
    }

    /// Set the icon size in pixels.
    pub fn icon_size(mut self, size: u32) -> Self {
        self.inner.icon_size = Some(size);
        self
    }

    /// Set the icon label text size in points.
    pub fn text_size(mut self, size: u32) -> Self {
        self.inner.text_size = Some(size);
        self
    }

    /// Add an icon position for a named entry.
    pub fn icon_position(mut self, name: String, x: i32, y: i32) -> Self {
        self.inner.icon_positions.push((name, x, y));
        self
    }

    /// Mark a file name's extension as hidden in Finder.
    pub fn hide_extension(mut self, name: String) -> Self {
        self.inner.hide_extensions.push(name);
        self
    }

    /// Attach an EULA file (plain text or RTF) as DMG license resources.
    pub fn eula(mut self, path: PathBuf) -> Self {
        self.inner.eula = Some(path);
        self
    }

    /// Build the [`DmgDecorations`].
    pub fn build(self) -> DmgDecorations {
        self.inner
    }
}

pub mod apfs;
pub mod ds_store;
pub mod gpt;
pub mod hfs;
pub mod zip_reader;

pub use gpt::{build_gpt_layout, GptLayout};
pub use hfs::build_hfs_plus_image_to_file;
pub use zip_reader::{ZipEntries, ZipEntry};
