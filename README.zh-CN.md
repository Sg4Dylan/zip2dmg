# zip2dmg

[**English**](README.md) | [**简体中文**](README.zh-CN.md)

将包含 macOS `.app` 包的 ZIP 压缩包转换为**未签名**的 DMG 磁盘镜像——以 Rust 实现，不依赖 macOS 系统、`hdiutil` 或任何外部工具。

## 工作原理

`zip2dmg` 参照 macOS 上 `hdiutil create` 的产出构建同构的磁盘镜像结构：

```
ZIP 压缩包
   │  解析条目（流式处理；文件数据不会完整驻留内存）
   ▼
文件系统镜像（HFS+ 或 APFS）
   │  通过文件级块设备写入，随后包装为
   ▼
GPT 分区布局
   │  MBR + 主/备 GPT 头与条目 + Apple_Free 间隙，
   │  布局与 hdiutil 产出的 8 分区结构一致
   ▼
UDIF DMG 容器
      zlib / bzip2 / LZFSE / Xz 压缩的数据分支、
      资源分支（可选 EULA 许可协议）及末尾的 koly 头
```

核心设计：

- **流式流水线** —— 文件内容从 ZIP 条目经 64 KiB 缓冲区直接流入块设备；峰值内存取决于工作集而非镜像总大小。
- **文件级块设备** —— 文件系统镜像写入临时文件而非内存，支持任意大小的应用。
- **Finder 装饰** —— 原生生成 `/Applications` 拖拽安装链接、背景图、卷图标、图标坐标、`.DS_Store` 窗口布局及 EULA 资源（无需 AppleScript 或 Finder 脚本）。
- **不做代码签名** —— 输出的 DMG 设计上即为未签名；请随后使用 [rcodesign](https://github.com/indygreg/apple-platform-rs) 或 `codesign` 签名。

## 基本用法

```bash
# 简单转换
zip2dmg --input MyApp.zip --output MyApp.dmg --volume-name MyApp

# 带 /Applications 拖拽链接与背景图的布局
zip2dmg --input MyApp.zip --output MyApp.dmg --volume-name MyApp \
    --app-drop-link 480,190 \
    --background background.png \
    --window-size 640,400 \
    --icon "MyApp.app,150,190"

# Bzip2 压缩与 APFS 文件系统
zip2dmg --input MyApp.zip --output MyApp.dmg --filesystem APFS --format UDBZ
```

主要选项：

| 选项 | 说明 |
| :--- | :--- |
| `--input` / `--output` | 输入 ZIP 与输出 DMG 路径 |
| `--volume-name` | Finder 中显示的卷名 |
| `--format` | `UDZO`（zlib，默认）、`UDBZ`（bzip2）、`ULFO`（LZFSE）、`ULMO`（xz）或 `raw` |
| `--filesystem` | `HFS+`（默认）或 `APFS` |
| `--app-drop-link` / `--ql-drop-link` | 创建用于拖拽安装的 `/Applications` / `/QuickLook` 符号链接 |
| `--background` / `--volume-icon` | 嵌入 Finder 背景图 / 卷图标（`.icns`） |
| `--window-pos` / `--window-size` / `--icon-size` | Finder 窗口布局 |
| `--icon` | 指定条目的图标坐标（`名称,x,y`，可多次使用） |
| `--add-file` | 向 DMG 追加额外文件（`源路径,目标路径[,x,y]`，可多次使用） |
| `--eula` | 附带纯文本或 RTF 许可协议 |

从源码构建：

```bash
cargo build --release
```

## 涉及的开源项目

本项目构建于以下开源组件之上，谨向原作者致谢。文件系统与 DMG 支持由下列项目的修补分支（fork）提供：

| 组件 | 许可证 | 用途 |
| :--- | :--- | :--- |
| [fstool](https://github.com/KarpelesLab/fstool) | MIT | HFS+ / APFS 文件系统构建、GPT/MBR 分区表 |
| [dpp](https://github.com/Dil4rd/dpp)（udif） | MIT | UDIF/DMG 容器写入 |
| [zip2](https://github.com/zip-rs/zip2) | MIT | ZIP 压缩包读取 |
| [create-dmg](https://github.com/sindresorhus/create-dmg) | MIT | EULA 资源模板参考 |
| [apple-platform-rs](https://github.com/indygreg/apple-platform-rs) | MPL-2.0 | 推荐搭配的签名工具（`rcodesign`） |
| [ds_store](https://github.com/sinistersnare/ds_store) | MIT | `.DS_Store` 格式参考与测试解析器 |

此外，CLI 与库还依赖 [clap](https://github.com/clap-rs/clap)、[anyhow](https://github.com/dtolnay/anyhow)、[log](https://github.com/rust-lang/log)、[env_logger](https://github.com/rust-cli/env_logger)、[uuid](https://github.com/uuid-rs/uuid)、[plist](https://github.com/ebarnard/rust-plist) 与 [base64](https://github.com/marshallpierce/rust-base64)（MIT / Apache-2.0）。

---

## 开源协议 (License)

### 项目协议
本项目遵循 **AGPLv3** 开源授权协议 —— 详情请参阅 [LICENSE](LICENSE) 文件。

### AI 生成内容披露与免责声明
本项目部分代码由 AI 工具生成或优化。尽管维护者努力确保代码质量，但 AI 生成的代码均以**“原样（AS IS）”**提供，不附带任何形式的明示或暗示保证。作者不保证 AI 贡献逻辑的绝对准确性、安全性或可靠性。建议用户在使用前自行审查源代码。在任何情况下，作者均不对因使用 AI 生成内容而产生的任何索赔、损害或其他责任负责。

### 第三方软件声明
本项目集成或使用了以下开源组件，在此向原作者表示感谢——完整列表及许可证信息请参阅上方 **「涉及的开源项目」** 表格。

---

Copyright (c) 2026-Present Sg4Dylan and project contributors.  
Licensed under the AGPLv3 License.
