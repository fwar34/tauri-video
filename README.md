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

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
