# zip2dmg

[**English**](README.md) | [**简体中文**](README.zh-CN.md)

Convert a ZIP archive containing a macOS `.app` bundle into an **unsigned** DMG disk image — implemented in Rust, with no dependency on macOS, `hdiutil`, or any external tools.

## How It Works

`zip2dmg` builds a disk image stack modeled on what `hdiutil create` produces on macOS:

```
ZIP archive
   │  parse entries (streaming; file data is never fully buffered in RAM)
   ▼
Filesystem image (HFS+ or APFS)
   │  written via a file-backed block device, then wrapped in
   ▼
GPT partition layout
   │  MBR + primary/backup GPT headers & entries + Apple_Free gaps,
   │  following the same 8-partition layout hdiutil produces
   ▼
UDIF DMG container
      zlib / bzip2 / LZFSE / XZ compressed data fork,
      resource fork (optional EULA), and the trailing `koly` header
```

Key design points:

- **Streaming pipeline** — file content flows from the ZIP entry through a 64 KiB buffer directly into the block device; peak memory is proportional to the working set, not the image size.
- **File-backed block device** — the filesystem image is built in a temp file instead of memory, so arbitrarily large apps are supported.
- **Finder decorations** — `/Applications` drop link, background image, volume icon, icon positions, `.DS_Store` window layout, and EULA resources are generated natively (no AppleScript, no Finder scripting).
- **No code signing** — the output DMG is unsigned by design; sign it afterwards with [rcodesign](https://github.com/indygreg/apple-platform-rs) or `codesign`.

## Basic Usage

```bash
# Simple conversion
zip2dmg --input MyApp.zip --output MyApp.dmg --volume-name MyApp

# Drag-install layout with /Applications symlink and background image
zip2dmg --input MyApp.zip --output MyApp.dmg --volume-name MyApp \
    --app-drop-link 480,190 \
    --background background.png \
    --window-size 640,400 \
    --icon "MyApp.app,150,190"

# Bzip2 compression and APFS filesystem
zip2dmg --input MyApp.zip --output MyApp.dmg --filesystem APFS --format UDBZ
```

Main options:

| Option | Description |
| :--- | :--- |
| `--input` / `--output` | Input ZIP and output DMG paths |
| `--volume-name` | Volume name shown in Finder |
| `--format` | `UDZO` (zlib, default), `UDBZ` (bzip2), `ULFO` (LZFSE), `ULMO` (xz), or `raw` |
| `--filesystem` | `HFS+` (default) or `APFS` |
| `--app-drop-link` / `--ql-drop-link` | Create `/Applications` / `/QuickLook` symlinks for drag-install |
| `--background` / `--volume-icon` | Embed a Finder background image / volume icon (`.icns`) |
| `--window-pos` / `--window-size` / `--icon-size` | Finder window layout |
| `--icon` | Position an item's icon (`name,x,y`, repeatable) |
| `--add-file` | Add extra files to the DMG (`src,target[,x,y]`, repeatable) |
| `--eula` | Attach a plain-text or RTF license agreement |

Build from source:

```bash
cargo build --release
```

## Related Open-Source Projects

This project is built on top of the following open-source components, with thanks to their authors. Filesystem and DMG support is provided by patched forks of these projects:

| Component | License | Usage |
| :--- | :--- | :--- |
| [fstool](https://github.com/KarpelesLab/fstool) | MIT | HFS+ / APFS filesystem construction, GPT/MBR partition tables |
| [dpp](https://github.com/Dil4rd/dpp) (udif) | MIT | UDIF/DMG container writing |
| [zip2](https://github.com/zip-rs/zip2) | MIT | ZIP archive reading |
| [create-dmg](https://github.com/sindresorhus/create-dmg) | MIT | Reference for EULA resource templates |
| [apple-platform-rs](https://github.com/indygreg/apple-platform-rs) | MPL-2.0 | Recommended companion tool for signing (`rcodesign`) |
| [ds_store](https://github.com/sinistersnare/ds_store) | MIT | `.DS_Store` format reference & test parser |

Additionally, the CLI and library depend on [clap](https://github.com/clap-rs/clap), [anyhow](https://github.com/dtolnay/anyhow), [log](https://github.com/rust-lang/log), [env_logger](https://github.com/rust-cli/env_logger), [uuid](https://github.com/uuid-rs/uuid), [plist](https://github.com/ebarnard/rust-plist), and [base64](https://github.com/marshallpierce/rust-base64) (MIT / Apache-2.0).

---

## License

### Project License
This project is licensed under the **AGPLv3 License** — see the [LICENSE](LICENSE) file for details.

### AI Disclosure & Disclaimer
Parts of this project are generated or optimized by AI coding tools. While the maintainers strive for quality, the AI-generated code is provided **"AS IS" without warranty of any kind**, express or implied. The authors do not guarantee the absolute accuracy, security, or reliability of the AI-contributed logic. Users are encouraged to review the source code independently. In no event shall the authors be liable for any claim, damages, or other liability arising from the use of AI-generated content.

### Third-Party Software Notices
This project integrates or makes use of the following open-source components, with gratitude to the original authors — see the table above under **Related Open-Source Projects** for the full list with licenses.

---

Copyright (c) 2026-Present Sg4Dylan and project contributors.  
Licensed under the AGPLv3 License.
