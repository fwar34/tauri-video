# Tauri + Vanilla TS

This template should help get you started developing with Tauri in vanilla HTML, CSS and Typescript.

## FFmpeg 依赖说明

应用通过**调用 FFmpeg 命令行程序**（`ffmpeg` / `ffprobe`）处理视频，编译期不链接 FFmpeg 开发库，
因此**不需要** `FFMPEG_DIR`、`pkg-config`、vcpkg 或 LLVM/libclang。

> 之前编译报 `error: FFmpeg not found.` 的原因：`ffmpeg-next` 属于 FFmpeg 绑定 crate，
> 编译时需要 FFmpeg 的**开发文件**（`include/` 头文件 + 导入库）。仅安装 `ffmpeg.exe`（例如 MSYS2 的
> `mingw-w64-ucrt-x86_64-ffmpeg`）并不满足要求，而且 MSYS2 的 mingw 导入库与 MSVC 工具链也不匹配。

运行时按以下顺序查找可执行文件（见 `src-tauri/src/video.rs`）：

1. 环境变量 `FFMPEG_PATH`（可指向 `ffmpeg.exe` 本身，或它所在的目录）
2. 系统 `PATH`
3. 常见安装目录常量 `FALLBACK_DIRS`（如 `M:\msys64\ucrt64\bin`、`C:\ffmpeg\bin`）

安装/自检：

```powershell
ffmpeg -version
ffprobe -version
```

验证解析逻辑与 ffprobe 集成：

```powershell
cd src-tauri
cargo test
```

## 使用 MSYS2 (GNU/UCRT64) 版 Rust 工具链（可选）

默认工具链是 `stable-x86_64-pc-windows-msvc`；也可以用 MSYS2 UCRT64 的 GNU 工具链
（`x86_64-pc-windows-gnu`）编译本项目，实测 dev / release 均可产出可运行的程序。

1. 安装 GNU 工具链（国内建议先设镜像，否则 static.rust-lang.org 很慢）：

```powershell
$env:RUSTUP_DIST_SERVER = 'https://mirrors.ustc.edu.cn/rust-static'
rustup toolchain install stable-x86_64-pc-windows-gnu
```

2. 链接器配置已写入 `src-tauri/.cargo/config.toml`（只用 MSYS2 的 gcc/ar，不依赖 PATH）：

```toml
[target.x86_64-pc-windows-gnu]
linker = "M:/msys64/ucrt64/bin/x86_64-w64-mingw32-gcc.exe"
ar = "M:/msys64/ucrt64/bin/ar.exe"
```

3. 构建（显式指定 `--target` 可让产物落在 `target/x86_64-pc-windows-gnu/`，
与 MSVC 的 `target/debug` 互不干扰）：

```powershell
cd src-tauri
cargo +stable-x86_64-pc-windows-gnu build --target x86_64-pc-windows-gnu
cargo +stable-x86_64-pc-windows-gnu build --release --target x86_64-pc-windows-gnu
```

想让它成为本项目默认（`npm run tauri dev` 也会用 GNU）：在 `src-tauri` 下执行
`rustup override set stable-x86_64-pc-windows-gnu`；全局切换用
`rustup default stable-x86_64-pc-windows-gnu`；回退用
`rustup override unset` / `rustup default stable-x86_64-pc-windows-msvc`。

### GNU 工具链的已知限制

- `[lib] crate-type` 必须是 `["rlib"]`（见 `src-tauri/Cargo.toml` 注释）。
  带 `cdylib`（Tauri 模板为移动端准备的）时 GNU 链接器会失败：
  `ld.exe: error: export ordinal too large`。需要打包 Android/iOS 时请改回
  `["staticlib", "cdylib", "rlib"]` 并使用 MSVC 工具链。
- `cargo test` 的集成测试程序在 GNU 下启动会报 `0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND`：
  它导入了 `comctl32.dll!TaskDialogIndirect`（只存在于 Common-Controls v6 清单激活的
  comctl32），而 `tauri-build` 只给主程序 exe 嵌入了该清单（测试 exe 里没有）。
  **跑测试请用默认的 MSVC 工具链**（`cargo test`），功能一致。
- GNU 产出的 exe 只依赖系统 DLL + `WebView2Loader.dll`（Rust 已静态链接 mingw 运行库），
  无需额外分发 libgcc/libwinpthread 等 DLL。

## Linux 构建依赖（Ubuntu / Debian）

Linux 下 Tauri 需要 GTK3 / WebKitGTK 的**开发包**（即提供 `.pc` 文件、供 `pkg-config` 查询的那些
`-dev` 包）。只装运行时库（例如 `libwebkit2gtk-4.1-0`）是不够的，`cargo build` 会在
`gdk-sys` / `gobject-sys` 的 build script 阶段失败：

```
The system library `gdk-3.0` required by crate `gdk-sys` was not found.
The file `gdk-3.0.pc` needs to be installed and the PKG_CONFIG_PATH environment variable
must contain its parent directory.
```

按 Tauri 2 官方列表一次性装齐（Ubuntu 22.04 / 24.04 / 25.10 等通用）：

```bash
sudo apt update
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

本项目未启用托盘（`tray-icon`）等特性，因此最小依赖其实只有
`libwebkit2gtk-4.1-dev`（会自动带上 `libgtk-3-dev`、`libsoup-3.0-dev`、
`libjavascriptcoregtk-4.1-dev`）加 `build-essential`；想与官方文档保持一致就装上面那一串。

安装后自检：

```bash
pkg-config --exists webkit2gtk-4.1 && echo OK
```

（`libxdo-dev` / `librsvg2-dev` / `libayatana-appindicator3-dev` 只在启用
`tauri/tray-icon`、`linux-libxdo` 等特性时才被链接，本项目的 `tauri = { version = "2", features = [] }`
用不到，但装上不会有副作用。）

### 运行时的 ffprobe

发行版的 `ffmpeg` 包会同时提供 `/usr/bin/ffmpeg` 与 `/usr/bin/ffprobe`：

```bash
sudo apt install ffmpeg
```

如果用的是 **snap 版 ffmpeg**，它只注册了 `ffmpeg`、`ffmpeg.ffplay`、`ffmpeg.ffprobe`
三个命令名，**没有 `ffprobe`**；而 `video.rs` 是按名字（`ffprobe`）查找可执行文件的，
于是 `start` 命令会返回 `未找到 ffprobe…`。两种解决办法：

1. 装发行版 ffmpeg（推荐，顺带避免 snap 与 apt 两份 ffmpeg 混用）；
2. 或用软链接补上正确的名字（`~/.local/bin` 通常在 `PATH` 中）：

```bash
mkdir -p ~/.local/bin
ln -sf /snap/bin/ffmpeg.ffprobe ~/.local/bin/ffprobe
```

也可以按上一节的方式，把 `FFMPEG_PATH` 指向同时含 `ffmpeg` 与 `ffprobe` 的目录。

### 首次运行

前端依赖同样要装（仓库里没有 `node_modules/`）：

```bash
npm install
npm run tauri dev
```

只跑 `cargo run` / `cargo build` 时不会加 `custom-protocol` 特性，Tauri 会按
`build.devUrl` 去加载 `http://localhost:1420`，所以需要另开一个终端跑 `npm run dev`；
用 `npm run tauri dev` 则两步一次完成。

在 Linux 上运行 Rust 测试：

```bash
cd src-tauri
cargo test
```

（`tests/ffmpeg_probe.rs` 会调用本机 ffmpeg 生成 1 秒样片再交给 ffprobe 解析；
本机完全没有 ffmpeg 时该用例会打印提示并跳过。注意“有 ffmpeg 但没有 ffprobe”
**不会**跳过，`probe_reads_generated_video` 会以 `ExecutableNotFound("ffprobe")` 失败。）

## VS Code 调试配置

仓库自带 `.vscode/launch.json` + `.vscode/tasks.json`（`.gitignore` 里已为这两个文件加了白名单）。
Rust 侧用 **CodeLLDB**（`vadimcn.vscode-lldb`，已在 `.vscode/extensions.json` 的推荐列表里），
前端侧用 VS Code 内置的 js-debug，都不需要额外装东西。按 `F5`（或调试面板）选择配置即可，
可以直接在 `src-tauri/src/*.rs` 里打断点、单步、查看变量。

| 配置 | 用途 |
| --- | --- |
| `Tauri: 开发调试 (自动启动 Vite)` | 先执行任务 `ui:dev`（即 `npm run dev`），再编译并启动 `src-tauri/target/debug/tauri-video`。日常开发用这个 |
| `Tauri: 开发调试 (复用已启动的 Vite)` | 已经在别的终端跑着 `npm run dev` 时用（Vite 配了 `strictPort`，重复启动会因 1420 被占用而失败） |
| `Tauri: 附加到运行中的进程` | 附加到已经在跑的进程，例如 `npm run tauri dev` 起的那个（Linux 见下面的 `ptrace_scope` 说明） |
| `Tauri: 前端调试 (WebView2 / 仅 Windows)` | 附加 WebView2 的 CDP 端口 9222，调试 `src/*.ts`、查看 WebView 控制台 |

附带的任务（`终端 → 运行任务`）：`ui:dev`、`ui:build`、`cargo:build`、`cargo:test`。
两个 `launch` 配置还注入了 `RUST_BACKTRACE=1`（panic 时打印完整回溯）和
`WEBKIT_INSPECTOR_SERVER=127.0.0.1:2999` / `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222`
（分别用于 Linux / Windows 的前端检查器，在另一个平台上是无害的空设置）。

### 为什么 Rust 调试必须先起 Vite

`tauri` crate 的 build script 里 `dev = !custom-protocol`（见 `tauri/src/build.rs`），
而本项目的 `Cargo.toml` 没有定义 `custom-protocol` 特性、`tauri.conf.json` 里也没在
`build.features` 中启用它，所以**直接 `cargo build` / `cargo build --release` 出来的可执行文件都是
dev 模式**：启动时按 `build.devUrl` 去加载 `http://localhost:1420`。
只有 Tauri CLI 打包时会启用 `tauri/custom-protocol` 把 `dist/` 内嵌进去（`npm run tauri build`），
`npm run tauri dev` 则本来就是加载 devUrl。

所以：用调试配置启动应用时，Vite 必须在跑；`preLaunchTask: ui:dev` 不能省，
否则窗口里只会显示连接失败。（要调试“内嵌前端资源”的产物，请在 `cargo build` 的参数里加上
`--features tauri/custom-protocol`，注意 `[profile.release]` 里 `strip = true`，release 断点基本不可用。）

### 前端（WebView）调试

- **Linux / WebKitGTK**：WebKitGTK 不支持 CDP，VS Code 无法附加。调试配置已设置
  `WEBKIT_INSPECTOR_SERVER=127.0.0.1:2999`，用浏览器打开 `http://127.0.0.1:2999`
  就是 WebKit 的检查器（等价于在窗口内右键 → 检查元素）。
- **Windows / WebView2**：调试配置已设置 `--remote-debugging-port=9222`，
  应用启动后选 `Tauri: 前端调试 (WebView2 / 仅 Windows)` 附加，即可对 `src/*.ts` 打断点
  （`webRoot` 已指向仓库根目录，与 Vite 的路径一致）。

### Linux 上附加到已有进程

Ubuntu 默认 `kernel.yama.ptrace_scope=1`，只允许附加到自己的子进程，
所以 `Tauri: 附加到运行中的进程` 附加别人（比如终端里 `npm run tauri dev`）启动的进程会报
`Operation not permitted`，临时放开即可：

```bash
sudo sysctl -w kernel.yama.ptrace_scope=0
```

上面两个 `launch` 配置不受影响：被调试进程是调试器的子进程。

### Windows 上换用 Visual Studio Windows Debugger（可选）

装了 C/C++ 扩展与 VS 的调试器后，也可以把 `type` 换成 `cppvsdbg`，用
`"program": "${workspaceFolder}/src-tauri/target/debug/tauri-video.exe"`，
`preLaunchTask` 等其余字段不变；官方文档见
<https://tauri.app/develop/debug/vscode/>。

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
