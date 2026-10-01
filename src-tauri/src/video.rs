use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::select;
use tokio::process::Command;
use tauri::ipc::{Channel, Response};

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
    #[error("tauri 错误")]
    TauriError(#[from] tauri::Error),
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

#[allow(dead_code)]
enum MessageType {
    Connected(String),
    DisConnected(String),
    Data(Vec<u8>),
}

#[allow(dead_code)]
struct Message(MessageType);

/// 一个起始码在缓冲区中的位置与长度（3 字节或 4 字节两种形式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StartCode {
    pos: usize,
    sc_len: usize,
}

/// 起始码的两种形式。`00 00 00 01` 必须优先匹配，否则会被识别成
/// 一个前导 0 加三字节起始码。
const SC4: [u8; 4] = [0x00, 0x00, 0x00, 0x01];
const SC3: [u8; 3] = [0x00, 0x00, 0x01];

/// 扫描缓冲区里**全部**起始码的位置。
///
/// 只在 `buf` 的整个有效范围上工作，长度由调用方通过切片表达，
/// 不再有"传进来的 len 和缓冲区有效长度对不上"的隐患。
///
/// 命中分支里 `i` 必须前进：原地不动会让同一个位置被无限次 push，
/// 几秒内把内存吃光（之前 OOM 的直接原因）。
fn find_all_start_codes(buf: &[u8]) -> Vec<StartCode> {
    let mut codes = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf[i..].starts_with(&SC4) {
            codes.push(StartCode { pos: i, sc_len: 4 });
            i += 4;
        } else if buf[i..].starts_with(&SC3) {
            codes.push(StartCode { pos: i, sc_len: 3 });
            i += 3;
        } else {
            i += 1;
        }
    }
    codes
}

/// 从累积缓冲区里切出**完整的** NALU（起始码 + 到下一个起始码之前的数据）。
///
/// 只把两个起始码之间的部分当作完整帧，所以开头那个起始码会留在剩余数据里，
/// 不会出现"把还没收齐的一帧当成完整帧发出去"。每个块自带起始码，
/// 前端可以直接 append 给 MSE 的 SourceBuffer。
///
/// `buf` 原地保留剩余字节（用 `drain` 前移，不做整块拷贝），下一轮直接继续 append。
/// 返回 (本轮的帧列表, 剩余字节数)。
fn parse_nals(buf: &mut Vec<u8>) -> (Vec<Vec<u8>>, usize) {
    let codes = find_all_start_codes(buf);
    if codes.len() < 2 {
        // 只有零个或一个起始码：这一帧什么时候结束还不知道，整体留到下一轮。
        return (Vec::new(), buf.len());
    }

    let mut chunks = Vec::with_capacity(codes.len() - 1);
    for pair in codes.windows(2) {
        let (start, end) = (pair[0].pos, pair[1].pos);
        chunks.push(buf[start..end].to_vec());
    }

    // 已发出去的部分整体丢弃，剩下的就是最后一个起始码之后、还没收齐的尾巴。
    let keep = codes[codes.len() - 1].pos;
    buf.drain(..keep);
    (chunks, buf.len())
}

/// 单条 IPC 消息的目标上限。按 NAL 一条条发会产生海量小消息（一个几分钟的
/// H.264 轻松几十万条），这是 Channel 丢消息和卡顿的主要来源；这里把小 NAL
/// 聚合到一批里发。超过上限的大 NAL（IDR 关键帧很常见）单独发，不会被截断。
const BATCH_TARGET: usize = 256 * 1024;

/// 发一批数据。发送失败不再被静默忽略，错误原样交给上层。
///
/// 刻意不打日志：这里每批都会走到，是一条几分钟的视频要跑几百上千次的热路径，
/// 逐批 println! 是同步 I/O，会把整个流水线拖慢。批数由调用方在结束时汇总打印。
fn send_batch<F>(batch: &mut Vec<u8>, flush: &mut F) -> Result<(), VideoError>
where
    F: FnMut(Vec<u8>) -> Result<(), VideoError>,
{
    if batch.is_empty() {
        return Ok(());
    }
    flush(std::mem::take(batch))
}

/// 把切好的 NAL 按 [`BATCH_TARGET`] 聚合成若干批发出去，返回实际发出的批数。
fn flush_chunks<F>(chunks: Vec<Vec<u8>>, flush: &mut F) -> Result<usize, VideoError>
where
    F: FnMut(Vec<u8>) -> Result<(), VideoError>,
{
    let mut batch: Vec<u8> = Vec::with_capacity(BATCH_TARGET);
    let mut batches = 0usize;
    for chunk in chunks {
        // 当前批已经装不下这一块，先吐出去（本身超过上限的单帧也就单独走一条）。
        if !batch.is_empty() && batch.len() + chunk.len() > BATCH_TARGET {
            send_batch(&mut batch, flush)?;
            batches += 1;
        }
        batch.extend_from_slice(&chunk);
    }
    if !batch.is_empty() {
        send_batch(&mut batch, flush)?;
        batches += 1;
    }
    Ok(batches)
}

async fn ffmpeg_h264(video_path: &str, on_chunk: Channel<Response>) -> Result<(), VideoError> {
    println!("ffmpeg_h264 enter...");
    let ffmpeg = resolve_program("ffmpeg")?;

    let mut command = Command::new(ffmpeg)
        .args([
            "-i", &video_path,
            "-c:v", "copy",
            "-f", "h264",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let out = command.stdout.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::BrokenPipe, format!("无法读取 ffmpeg 的 stdout"))
    })?;

    let err = command.stderr.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::BrokenPipe, format!("无法读取 ffmpeg 的 stderr"))
    })?;

    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut out_reader = BufReader::new(out);
    let mut err_reader = BufReader::new(err);

    // 待组帧的数据：上一轮没切完的尾巴 + 本轮新读到的字节。
    // 容量只有"一帧大小"量级，不随视频时长增长。
    let mut pending: Vec<u8> = Vec::new();
    let mut read_buf = [0u8; 64 * 1024];
    let mut line_string = String::new();
    let mut sent_frames = 0usize;
    let mut sent_batches = 0usize;

    while stdout_open || stderr_open {
        select! {
            // stdout：二进制 H.264 流
            res = out_reader.read(&mut read_buf), if stdout_open => {
                match res {
                    Ok(0) => {
                        // ffmpeg 正常收尾。这里只关掉 stdout，绝不能 break：
                        // break 会让 stderr 管道没人读，写满后 wait() 永久挂住。
                        stdout_open = false;
                        continue;
                    }
                    Ok(n) => {
                        pending.extend_from_slice(&read_buf[..n]);

                        let (chunks, _) = parse_nals(&mut pending);

                        sent_frames += chunks.len();
                        let mut flush = |bytes: Vec<u8>| -> Result<(), VideoError> {
                            on_chunk
                                .send(Response::new(bytes))
                                .map_err(VideoError::TauriError)
                        };
                        sent_batches += flush_chunks(chunks, &mut flush)?;
                        // 只在第一次发送时留一行诊断：如果终端里只有这一行、
                        // 后面再也没有 "done"，说明 send 卡住了（不是 JS 丢消息）。
                        if sent_batches == 1 {
                            println!("first batch sent, read len so far:{n}");
                        }
                    }
                    Err(e) => {
                        eprintln!("stdout err: {e}");
                        stdout_open = false;
                    }
                }
            }

            // stderr：ffmpeg 的文本日志，只打印，不参与组帧
            res = err_reader.read_line(&mut line_string), if stderr_open => {
                match res {
                    Ok(0) => stderr_open = false,
                    Ok(_) => {
                        eprintln!("ffmpeg {}", line_string.trim_end());
                        line_string.clear();
                    }
                    Err(e) => {
                        stderr_open = false;
                        eprintln!("stderr err: {e}")
                    }
                }
            }
        }
    }

    let status = command.wait().await?;
    println!("ffmpeg_h264 done, sent {sent_frames} frames in {sent_batches} batches, exit: {status}");

    // 收尾：流结束后 pending 里剩下的就是最后一帧（它只是缺"下一个起始码"这个
    // 结束边界而已），必须发出去，否则整段视频会丢掉尾部一帧。
    if !pending.is_empty() {
        println!("flush tail {} bytes", pending.len());
        let tail = std::mem::take(&mut pending);
        on_chunk.send(Response::new(tail))?;
    }

    Ok(())
}

/// 校验文件存在并读取基本信息，作为播放 / 处理前的准备步骤。
pub async fn start(video_path: &str, on_chunk: Channel<Response>) -> Result<MediaInfo, VideoError> {
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

    ffmpeg_h264(video_path, on_chunk).await?;

    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_both_start_code_forms() {
        let buf = [0xFF, 0x00, 0x00, 0x00, 0x01, 0xAB, 0x00, 0x00, 0x01, 0xCD];
        let codes = find_all_start_codes(&buf);
        let actual: Vec<(usize, usize)> = codes.iter().map(|c| (c.pos, c.sc_len)).collect();
        assert_eq!(actual, vec![(1, 4), (6, 3)]);
    }

    #[test]
    fn no_start_code_is_empty() {
        assert!(find_all_start_codes(&[0x11, 0x22, 0x33]).is_empty());
    }

    #[test]
    fn four_byte_start_code_wins_over_three_byte() {
        // 连续 5 个 0 后面跟 1：只有一个 4 字节起始码，不能被拆成 3+1。
        let buf = [0x00, 0x00, 0x00, 0x00, 0x01];
        let codes = find_all_start_codes(&buf);
        assert_eq!(codes.len(), 1);
        assert_eq!((codes[0].pos, codes[0].sc_len), (1, 4));
    }

    #[test]
    fn parse_keeps_incomplete_tail() {
        // 只有一个起始码：这一帧还没结束，整体留在缓冲区里，不能发出去。
        let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA];
        let (chunks, remaining) = parse_nals(&mut buf);
        assert!(chunks.is_empty());
        assert_eq!(remaining, 6);
        assert_eq!(buf, vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA]);
    }

    #[test]
    fn parse_splits_nals_and_keeps_last() {
        // 两个完整 NALU 边界 + 一个没收齐的尾巴。
        let payload_a = [0x67, 0xAA, 0xBB];
        let payload_b = [0x68, 0xCC];
        let tail = [0x65, 0xDD];
        let mut buf = Vec::new();
        buf.extend_from_slice(&SC4);
        buf.extend_from_slice(&payload_a);
        buf.extend_from_slice(&SC3);
        buf.extend_from_slice(&payload_b);
        buf.extend_from_slice(&SC4);
        buf.extend_from_slice(&tail);

        let (chunks, remaining) = parse_nals(&mut buf);

        let mut expected_first = SC4.to_vec();
        expected_first.extend_from_slice(&payload_a);
        let mut expected_second = SC3.to_vec();
        expected_second.extend_from_slice(&payload_b);
        assert_eq!(chunks, vec![expected_first, expected_second]);

        let mut expected_tail = SC4.to_vec();
        expected_tail.extend_from_slice(&tail);
        assert_eq!(remaining, expected_tail.len());
        assert_eq!(buf, expected_tail);
    }

    #[test]
    fn parse_reassembles_split_start_code_across_reads() {
        // 起始码被切在两次读取之间：第一块以一个孤立的 0 结尾，
        // 第二块补上 00 00 01。跨块拼接后必须能识别出来。
        let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0x00];
        let (chunks, remaining) = parse_nals(&mut buf);
        assert!(chunks.is_empty());
        assert_eq!(remaining, 7);
        assert_eq!(buf, vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0x00]);

        buf.extend_from_slice(&[0x00, 0x00, 0x01, 0x68]);
        let (chunks, remaining) = parse_nals(&mut buf);
        // 第一个 NALU 现在完整了（起始码 + 0x67 0xAA），
        // 剩下的 0x00 是分隔零字节，会被留在下一个起始码之前。
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0x00]);
        assert_eq!(remaining, 4);
        assert_eq!(buf, vec![0x00, 0x00, 0x01, 0x68]);
    }

    #[test]
    fn batches_small_nals_together() {
        // 两个小 NAL 会被合并成一批，消息数从 2 降到 1。
        let chunks = vec![vec![1u8; 10], vec![2u8; 20]];
        let mut sent: Vec<Vec<u8>> = Vec::new();
        {
            let mut flush = |bytes: Vec<u8>| -> Result<(), VideoError> {
                sent.push(bytes);
                Ok(())
            };
            let batches = flush_chunks(chunks, &mut flush).unwrap();
            assert_eq!(batches, 1);
        }
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].len(), 30);
    }

    #[test]
    fn batching_splits_at_target_and_never_truncates() {
        // 三块略小于目标的数据，应当分成两批；总字节数必须与输入一致。
        let each = BATCH_TARGET / 3 + 1;
        let chunks = vec![vec![1u8; each], vec![2u8; each], vec![3u8; each]];
        let total: usize = each * 3;
        let mut sent: Vec<Vec<u8>> = Vec::new();
        {
            let mut flush = |bytes: Vec<u8>| -> Result<(), VideoError> {
                sent.push(bytes);
                Ok(())
            };
            flush_chunks(chunks, &mut flush).unwrap();
        }
        assert_eq!(sent.iter().map(|b| b.len()).sum::<usize>(), total);
        assert!(sent.len() >= 2);
        // 除去可能单独成批的超标块，任何一批都不应超过目标太多。
        assert!(sent.iter().all(|b| b.len() <= BATCH_TARGET + each));
    }

    #[test]
    fn oversized_single_chunk_is_sent_alone() {
        // 单个超过目标的 NAL（IDR 帧很常见）必须单独一条发，不能被截断或丢弃。
        let big = BATCH_TARGET + 1024;
        let chunks = vec![vec![7u8; big], vec![8u8; 16]];
        let mut sent: Vec<Vec<u8>> = Vec::new();
        {
            let mut flush = |bytes: Vec<u8>| -> Result<(), VideoError> {
                sent.push(bytes);
                Ok(())
            };
            let batches = flush_chunks(chunks, &mut flush).unwrap();
            assert_eq!(batches, 2);
        }
        assert_eq!(sent.iter().map(|b| b.len()).sum::<usize>(), big + 16);
        assert!(sent.iter().any(|b| b.len() == big));
    }
}