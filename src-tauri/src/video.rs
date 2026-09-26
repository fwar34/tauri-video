use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// FFmpeg 不在 `PATH` 中时，依次尝试这些常见安装目录。
const FALLBACK_DIRS: [&str; 3] = [
    r"M:\msys64\ucrt64\bin",
    r"C:\ffmpeg\bin",
    r"C:\Program Files\ffmpeg\bin",
];

#[derive(Debug, Error)]
pub enum VideoError {
    #[error("文件未找到: {0}")]
    FileNotFound(String),
    #[error("未找到 {0}，请安装 FFmpeg 或用 FFMPEG_PATH 指定它所在目录")]
    ExecutableNotFound(String),
    #[error("IO 错误: {0}")]
    Io(#[from] io::Error),
    #[error("解析 ffprobe 输出失败: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{program} 执行失败（退出码 {code}）: {stderr}")]
    CommandFailed {
        program: &'static str,
        code: i32,
        stderr: String,
    },
}

impl Serialize for VideoError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// 由 `ffprobe` 读取到的媒体信息，可直接返回给前端。
#[derive(Debug, Clone, Serialize)]
pub struct MediaInfo {
    /// 时长（秒）
    pub duration: f64,
    /// 文件大小（字节）
    pub size: u64,
    /// 容器格式，例如 `mov,mp4,m4a,3gp,3g2,mj2`
    pub format: String,
    /// 视频流编码，例如 `h264`
    pub video_codec: Option<String>,
    /// 音频流编码，例如 `aac`
    pub audio_codec: Option<String>,
}

/// Windows 上可执行文件带 `.exe` 后缀，其它平台使用原名。
fn executable_names(name: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{name}.exe"), name.to_string()]
    } else {
        vec![name.to_string()]
    }
}

/// 在指定目录下查找可执行文件。
fn find_in_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    executable_names(name)
        .into_iter()
        .map(|exe| dir.join(exe))
        .find(|path| path.is_file())
}

/// 解析 `ffmpeg` / `ffprobe` 可执行文件路径，查找顺序：
/// 1. 环境变量 `FFMPEG_PATH`（可指向可执行文件本身或其所在目录）
/// 2. 系统 `PATH`
/// 3. 常见安装目录 [`FALLBACK_DIRS`]
pub fn resolve_program(name: &str) -> Result<PathBuf, VideoError> {
    if let Some(raw) = std::env::var_os("FFMPEG_PATH") {
        let candidate = PathBuf::from(raw);
        if candidate.is_file() {
            if let Some(found) = candidate.parent().and_then(|dir| find_in_dir(dir, name)) {
                return Ok(found);
            }
        } else if let Some(found) = find_in_dir(&candidate, name) {
            return Ok(found);
        }
    }

    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if let Some(found) = find_in_dir(&dir, name) {
                return Ok(found);
            }
        }
    }

    for dir in FALLBACK_DIRS {
        if let Some(found) = find_in_dir(Path::new(dir), name) {
            return Ok(found);
        }
    }

    Err(VideoError::ExecutableNotFound(name.to_string()))
}

/// 逐行读取管道内容（保留换行），用于收集子进程输出。
async fn read_lines<R>(reader: R) -> io::Result<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut text = String::new();
    while let Some(line) = lines.next_line().await? {
        text.push_str(&line);
        text.push('\n');
    }
    Ok(text)
}

/// 运行外部程序并返回它的 stdout；失败时把 stderr 作为错误信息。
async fn run_capture(
    program_name: &'static str,
    program: &Path,
    args: &[&str],
) -> Result<String, VideoError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    // 先把管道取出来，否则管道缓冲区写满会让子进程一直阻塞。
    let stdout = child.stdout.take().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("无法读取 {program_name} 的 stdout"),
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("无法读取 {program_name} 的 stderr"),
        )
    })?;

    // stdout 与 stderr 必须同时读取，只消费一侧会造成死锁。
    let (stdout_text, stderr_text) = tokio::try_join!(read_lines(stdout), read_lines(stderr))?;
    let status = child.wait().await?;

    if !status.success() {
        return Err(VideoError::CommandFailed {
            program: program_name,
            code: status.code().unwrap_or(-1),
            stderr: stderr_text.trim().to_string(),
        });
    }

    Ok(stdout_text)
}

/// 调用 `ffprobe` 读取媒体信息（时长、大小、容器与编解码器）。
pub async fn probe(video_path: &Path) -> Result<MediaInfo, VideoError> {
    let ffprobe = resolve_program("ffprobe")?;
    let input = video_path.to_string_lossy().into_owned();

    let json = run_capture(
        "ffprobe",
        &ffprobe,
        &[
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            &input,
        ],
    )
    .await?;

    let value: serde_json::Value = serde_json::from_str(&json)?;
    let format = value.get("format").unwrap_or(&serde_json::Value::Null);

    let duration = format
        .get("duration")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);
    let size = format
        .get("size")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let format_name = format
        .get("format_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let mut video_codec = None;
    let mut audio_codec = None;
    if let Some(streams) = value.get("streams").and_then(|v| v.as_array()) {
        for stream in streams {
            let codec_name = stream
                .get("codec_name")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            match stream.get("codec_type").and_then(|v| v.as_str()) {
                Some("video") if video_codec.is_none() => video_codec = codec_name,
                Some("audio") if audio_codec.is_none() => audio_codec = codec_name,
                _ => {}
            }
        }
    }

    Ok(MediaInfo {
        duration,
        size,
        format: format_name,
        video_codec,
        audio_codec,
    })
}

/// 校验文件存在并读取基本信息，作为播放 / 处理前的准备步骤。
pub async fn start(video_path: &str) -> Result<MediaInfo, VideoError> {
    let path = Path::new(video_path);
    if !path.is_file() {
        return Err(VideoError::FileNotFound(video_path.to_string()));
    }

    let info = probe(path).await?;
    println!(
        "[video] {} -> {:.2}s, {} 字节, 容器 {}, 视频 {}, 音频 {}",
        video_path,
        info.duration,
        info.size,
        info.format,
        info.video_codec.as_deref().unwrap_or("无"),
        info.audio_codec.as_deref().unwrap_or("无"),
    );

    Ok(info)
}