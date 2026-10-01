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
#[serde(rename_all = "camelCase")]
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
    /// 视频宽（像素），用于前端提前初始化画布
    pub width: Option<u32>,
    /// 视频高（像素）
    pub height: Option<u32>,
    /// 真实帧率，用于计算每帧的显示时长；探测不到时为 None
    pub frame_rate: Option<f64>,
}

impl MediaInfo {
    /// 每帧显示时长（微秒）。WebCodecs 的 timestamp/duration 都用微秒。
    ///
    /// 帧率读不到时退回 30fps，但 `.max(1.0)` 保证不会除出 0 或负值。
    pub fn frame_duration_micros(&self) -> f64 {
        1_000_000.0 / self.frame_rate.unwrap_or(30.0).max(1.0)
    }
}

/// 解析 ffprobe 的帧率字符串，例如 `"30000/1001"` → 29.97。
///
/// 优先用 `avg_frame_rate`（整段平均，对 VFR 更有代表性），
/// 它为空或 `"0/0"` 时退回 `r_frame_rate`。
fn parse_frame_rate(stream: &serde_json::Value) -> Option<f64> {
    for key in ["avg_frame_rate", "r_frame_rate"] {
        let Some(raw) = stream.get(key).and_then(|v| v.as_str()) else {
            continue;
        };
        let Some((num, den)) = raw.split_once('/') else {
            continue;
        };
        let (Ok(num), Ok(den)) = (num.parse::<f64>(), den.parse::<f64>()) else {
            continue;
        };
        if den > 0.0 && num > 0.0 {
            return Some(num / den);
        }
    }
    None
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
    let mut width = None;
    let mut height = None;
    let mut frame_rate = None;
    if let Some(streams) = value.get("streams").and_then(|v| v.as_array()) {
        for stream in streams {
            let codec_name = stream
                .get("codec_name")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            match stream.get("codec_type").and_then(|v| v.as_str()) {
                Some("video") if video_codec.is_none() => {
                    video_codec = codec_name;
                    width = stream.get("width").and_then(|v| v.as_u64()).map(|v| v as u32);
                    height = stream.get("height").and_then(|v| v.as_u64()).map(|v| v as u32);
                    frame_rate = parse_frame_rate(stream);
                }
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
        width,
        height,
        frame_rate,
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

/// 从累积缓冲区里切出**确定完整**的 NAL。
///
/// 一个 NAL 的结束位置只能由"下一个起始码"确定，所以缓冲区里最后那个起始码
/// 之后的数据一律不算完整——它的长度可能还没读全。这部分作为 `carry` 原样
/// 交还给调用方，必须**原封不动**地在下一次解析时接回缓冲区前面。
///
/// 关键点：已经返回的 NAL 字节会从缓冲区里 drain 掉，绝不会再被解析一次。
/// 如果既把它们的字节留着、又把它们作为 NAL 交出去，就会出现"同一个 NAL 被
/// 推两遍"的重复，帧边界和长度全部错位（真实日志里 AU#0 len=0 就是这么来的）。
///
/// 返回 `(完整的 NAL 列表, 未完成的尾巴字节)`。
fn parse_nals(buf: &mut Vec<u8>) -> (Vec<VideoNal>, Vec<u8>) {
    let codes = find_all_start_codes(buf);
    if codes.len() < 2 {
        // 一个完整的 NAL 都确定不下来，整段留到下一轮。
        return (Vec::new(), std::mem::take(buf));
    }

    let mut nals = Vec::with_capacity(codes.len() - 1);
    for pair in codes.windows(2) {
        let (start, end) = (pair[0].pos, pair[1].pos);
        let data = buf[start..end].to_vec();
        let header = buf.get(start + pair[0].sc_len).copied().unwrap_or(0);
        nals.push(VideoNal {
            nal_type: header & 0x1f,
            ref_idc: (header >> 5) & 0x03,
            data,
        });
    }

    // 最后一个起始码之后的部分还没定界，单独交出去；前半段已经切走了。
    let carry = buf.split_off(codes[codes.len() - 1].pos);
    (nals, carry)
}

/// 一个解析好的 NAL：裸数据 + 头字节拆出来的两个字段。
#[derive(Clone)]
struct VideoNal {
    nal_type: u8,
    /// `nal_ref_idc != 0` 表示这个 NAL 参与参考，是判断随机访问点的一半依据。
    ref_idc: u8,
    data: Vec<u8>,
}

/// 单条 IPC 消息的目标字节上限。
///
/// 把若干帧塞进同一条消息是为了压低 IPC 次数——每帧一条消息在几分钟的视频上
/// 就是几十万次往返，这正是 Channel 丢消息和卡顿的主要来源。单帧超过这个上限
/// （IDR 关键帧很常见）时它自己单独成批，不会被截断。
const BATCH_TARGET: usize = 256 * 1024;

/// 组帧缓冲的告警阈值。正常一帧远小于它；超过说明遇到了异常大的帧或
/// 帧边界识别失败，至少要在日志里看得见，而不是静默地一直吃内存。
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

/// VCL NAL（type 1..=5）才是图像数据；5 是 IDR。
fn is_vcl(nal_type: u8) -> bool {
    (1..=5).contains(&nal_type)
}

/// 跳过 emulation prevention 字节（`00 00 03` 里的 `03`），取出 RBSP 第 `index` 个字节。
///
/// slice header 里紧跟在头字节后面的那个 `ue(v)`（`first_mb_in_slice`）通常
/// 落在头两个字节内，所以只需要取一两个字节就够判断 slice_type 了。
fn rbsp_byte(payload: &[u8], index: usize) -> Option<u8> {
    let mut rbsp_index = 0usize;
    let mut i = 0usize;
    while i < payload.len() {
        // 03 是防竞争字节，编码时插进来的，读取时要去掉。
        if i >= 2 && payload[i] == 0x03 && payload[i - 1] == 0x00 && payload[i - 2] == 0x00 {
            i += 1;
            continue;
        }
        if rbsp_index == index {
            return Some(payload[i]);
        }
        rbsp_index += 1;
        i += 1;
    }
    None
}

/// 取 RBSP 里第 `bit_index` 个比特（从最高位开始排序）。
fn rbsp_bit(payload: &[u8], bit_index: usize) -> Option<u32> {
    let byte = rbsp_byte(payload, bit_index / 8)?;
    Some(((byte >> (7 - (bit_index % 8))) & 1) as u32)
}

/// 从 RBSP 指定比特位置读一个 `ue(v)`（指数哥伦布无符号数），返回 (值, 新位置)。
fn read_ue(payload: &[u8], mut bit_index: usize) -> Option<(u32, usize)> {
    let mut leading_zeros = 0usize;
    while rbsp_bit(payload, bit_index)? == 0 {
        leading_zeros += 1;
        bit_index += 1;
        // 到 32 位还没读到 1，说明数据已经不对了，别无限读下去。
        if leading_zeros > 32 {
            return None;
        }
    }
    bit_index += 1; // 跳过那个 1

    let mut value = 0u32;
    for _ in 0..leading_zeros {
        value = (value << 1) | rbsp_bit(payload, bit_index)?;
        bit_index += 1;
    }
    Some(((1u32 << leading_zeros) - 1 + value, bit_index))
}

/// 解析 slice 的 `slice_type`。
///
/// slice header 开头依次是 `first_mb_in_slice`(ue) 和 `slice_type`(ue)，
/// 所以先读一个 ue 丢掉，再读第二个。
fn parse_slice_type(nal: &[u8]) -> Option<u32> {
    let payload = payload_after_header(nal)?;
    let (_, after_first) = read_ue(payload, 0)?;
    let (slice_type, _) = read_ue(payload, after_first)?;
    Some(slice_type % 5)
}

/// 取 NAL 头字节之后的数据（RBSP 起点）。
fn payload_after_header(nal: &[u8]) -> Option<&[u8]> {
    if nal.starts_with(&SC4) {
        nal.get(5..)
    } else if nal.starts_with(&SC3) {
        nal.get(4..)
    } else {
        None
    }
}

/// 是否是 I slice（`slice_type` 2 或 7，模 5 之后都是 2）。
fn is_i_slice(slice_type: u32) -> bool {
    slice_type == 2
}

/// 一个 NAL 本身能不能作为解码起点。
///
/// 不能只看 `nal_unit_type == 5`（IDR）：很多编码器（例如 Maconcept / Main profile）
/// 用**非 IDR 的 I 帧**做开头和随机访问点，它们的 `nal_unit_type` 是 1，
/// 只看 IDR 会把这种真正的关键帧判成 delta，解码器立刻就报
/// "A key frame is required after configure()"。
///
/// 判据（参照 H.264 7.4.1.2.3 的随机访问点定义，取视频可用的一侧）：
/// - IDR slice（type 5）→ 是
/// - 参与参考（`nal_ref_idc != 0`）的 I slice（type 1 且 slice_type 为 I）→ 是
/// - 非参考的 I slice → 不算，它后面的帧不依赖它但也不保证能当起点
fn nal_starts_decoding_at(nal: &VideoNal) -> bool {
    if nal.nal_type == 5 {
        return true;
    }
    if nal.nal_type != 1 || nal.ref_idc == 0 {
        return false;
    }
    parse_slice_type(&nal.data).is_some_and(is_i_slice)
}

/// 该 NAL 是否让新的访问单元开始（规范 7.4.1.2.4 的实用简化版）。
///
/// 调用方只在"当前 AU 里已经出现过 VCL"时才问这个问题，所以这里不用再判断：
/// - 又遇到一个 VCL → 新 AU（多 slice 的情形）
/// - 遇到 AUD/SPS/PPS/SEI → 新 AU
fn starts_new_access_unit(nal_type: u8) -> bool {
    is_vcl(nal_type) || matches!(nal_type, 6 | 7 | 8 | 9)
}

/// 把 Annex-B 的 NAL 序列聚合成访问单元（帧）。
///
/// 一帧通常包含多个 NAL（SPS + PPS + SEI + slice，多 slice 时还有多个 VCL），
/// 而 WebCodecs 的 `EncodedVideoChunk` 语义上就是"一个 access unit"，
/// 所以必须在发送前聚合，不能一个 NAL chunk 一次 decode。
///
/// **最后一个 AU 一律不发**：它的最后一个 NAL 的长度要靠"后续起始码"确定，
/// 而那个起始码可能还在管道另一头；多 slice 的帧也可能还有 slice 没到齐。
/// 把它的**原始字节**原样返回，下一轮接上新数据再切。
///
/// 这条规则是必须的，不是保守：ffmpeg 的 muxer 按 32KB 边界写出，read 边界
/// 几乎总是落在帧中间，把"最后一个 AU"直接发出去就等于每帧都发半帧
/// （真实日志里 AU#0 只有 310855 字节、slice 只剩个头，就是这么来的）。
///
/// `start_timestamp`/`duration`：第 `i` 个发出的 AU 时间戳是
/// `start_timestamp + i * duration`——一次 read 里出现多个 AU 很正常，
/// 它们必须拿到各不相同的时间戳。
///
/// 返回 `(完整 AU 列表, 最后一个未完成 AU 的原始字节)`。
/// 返回的字节里不含任何已经发出去的内容，直接 append 新数据即可继续。
fn group_access_units(
    nals: Vec<VideoNal>,
    start_timestamp: f64,
    duration: f64,
) -> (Vec<VideoPacket>, Vec<u8>) {
    let mut units: Vec<VideoPacket> = Vec::new();
    let mut builder = AccessUnitBuilder::default();

    for nal in nals {
        // 只有"已经有图像数据"的 AU 才允许切出去。
        //
        // 不能只看 has_vcl()：SPS/PPS/SEI 这些非 VCL NAL 会先到，如果这时候
        // 就因为它们后面跟了 slice 而切分，切出来的 chunk 里一个 slice 都没有。
        // 解码器拿到这种"标记为 key 却没有图像数据"的 chunk 会直接进入
        // "A key frame is required" 状态，后面所有帧都解不出来。
        // Annex-B 的参数集本来就是带内的，必须和它后面的 slice 一起送。
        if builder.has_picture_body() && starts_new_access_unit(nal.nal_type) {
            let timestamp = start_timestamp + units.len() as f64 * duration;
            units.push(builder.finish(timestamp, duration));
        }
        builder.push(&nal);
    }

    // 剩下的是最后一个 AU：它有 VCL 说明数据基本够但边界还没定，
    // 没 VCL 说明只是个开头。两种都留到下一轮，绝不在这里发出去。
    (units, builder.into_bytes())
}

/// 正在拼装的访问单元：按字节偏移记录每个 VCL NAL 的起点。
///
/// 用偏移而不是克隆 NAL，是为了切分时不用把所有数据复制一遍。
#[derive(Default)]
struct AccessUnitBuilder {
    /// 已经拼进去的 Annex-B 字节。
    bytes: Vec<u8>,
    /// 每个 VCL NAL 在 `bytes` 里的起始偏移，天然递增。
    vcl_starts: Vec<usize>,
    /// 每个 VCL NAL 是否构成随机访问点，和 `vcl_starts` 一一对应。
    vcl_keys: Vec<bool>,
}

impl AccessUnitBuilder {
    /// 当前 AU 里是否已经有图像数据（VCL NAL）。
    fn has_picture_body(&self) -> bool {
        !self.vcl_starts.is_empty()
    }

    fn push(&mut self, nal: &VideoNal) {
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(&nal.data);
        if is_vcl(nal.nal_type) {
            self.vcl_starts.push(offset);
            self.vcl_keys.push(nal_starts_decoding_at(nal));
        }
    }

    /// 切出当前 AU 并把 builder 清空，准备接收下一帧。
    ///
    /// 用 `&mut self` 而不是 `self`：同一个循环里要反复切帧，取所有权会在
    /// 第二次调用时报 "value used here after move"。
    fn finish(&mut self, timestamp: f64, duration: f64) -> VideoPacket {
        // 清空之前先取关键帧标记。
        let key = self.vcl_keys.iter().any(|k| *k);
        // 一个 AU 就是 builder 里累积的全部字节。这里不能截到"最后一个 VCL 的起点"：
        // 那会把真正的 slice 切掉（真实表现就是帧里只剩参数集，长度 14 而不是 22）。
        // 调用方只在"下一个 AU 的起点"出现时才调 finish，所以此刻缓冲区里的内容
        // 正好是完整的一帧。
        let data = std::mem::take(&mut self.bytes);
        self.vcl_starts.clear();
        self.vcl_keys.clear();
        VideoPacket {
            data,
            timestamp,
            duration,
            key,
        }
    }

    /// 把整个缓冲区作为"下一轮的残留"交出去。
    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// 不设 `NAL_DEBUG` 时默认打印多少个 AU 的结构。
///
/// 逐帧 println! 是同步 I/O，几百帧刷下来会明显拖慢流水线（实测 ffmpeg 的
/// speed 会从 1.0x 掉到 0.72x），所以默认只打开头几个——它们足以回答
/// "关键帧有没有被识别到""参数集有没有被单独成帧"这两个最常见的问题。
const DIAG_FRAMES: usize = 8;

/// 诊断输出的档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Diag {
    /// 关闭。
    Off,
    /// 只打前 [`DIAG_FRAMES`] 个 AU（默认）。
    FirstFrames,
    /// 每一帧都打（`NAL_DEBUG=all`，会明显拖慢）。
    All,
}

/// 读取诊断档位：`NAL_DEBUG=all` 全量，`NAL_DEBUG=0/off` 关闭，
/// 其它值（含未设置）都是默认档。
fn diag_level() -> Diag {
    match std::env::var("NAL_DEBUG").ok().as_deref() {
        Some("all") => Diag::All,
        Some("0") | Some("off") | Some("false") => Diag::Off,
        _ => Diag::FirstFrames,
    }
}

/// 取一个 AU 里所有 NAL 的 `nal_unit_type`，用于诊断输出。
fn collect_nal_types(unit: &[u8]) -> Vec<u8> {
    find_all_start_codes(unit)
        .iter()
        .filter_map(|code| unit.get(code.pos + code.sc_len).map(|header| header & 0x1f))
        .collect()
}

/// 把 NAL 类型序列和关键帧标记格式化成一行，例如 `[7,8,6,1] key=true`。
///
/// 排查 "A key frame is required" 这类问题时，这一行是唯一能直接看出
/// "送出去的到底是什么"的东西：参数集有没有被单独成帧、IDR 有没有被漏标、
/// slice 是不是完整的，全都在这上面。
fn describe_access_unit(nal_types: &[u8], key: bool) -> String {
    let types: Vec<String> = nal_types.iter().map(|t| t.to_string()).collect();
    format!("[{}] key={key}", types.join(","))
}

/// 打印一个即将发出的 AU 的结构。
fn trace_access_unit(index: usize, packet: &VideoPacket) {
    match diag_level() {
        Diag::Off => return,
        Diag::All => {}
        Diag::FirstFrames if index >= DIAG_FRAMES => return,
        Diag::FirstFrames => {}
    }
    let types = collect_nal_types(&packet.data);
    println!(
        "[video] AU#{index} ts={:.0} len={} {}",
        packet.timestamp,
        packet.data.len(),
        describe_access_unit(&types, packet.key),
    );
}

/// 默认档打印完前 [`DIAG_FRAMES`] 帧后提示一次，说明后面不再打印。
///
/// 用调用方持有的标志位而不是比较帧数：一次 read 可能吐出多个 AU，
/// 帧数会跳过 `DIAG_FRAMES`，靠等值判断就永远不触发。
fn trace_truncated_note(sent_frames: usize, note_printed: &mut bool) {
    if *note_printed || diag_level() != Diag::FirstFrames || sent_frames <= DIAG_FRAMES {
        return;
    }
    *note_printed = true;
    println!("[video] （以上只显示前 {DIAG_FRAMES} 帧；NAL_DEBUG=all 可输出全部）");
}

/// 从 SPS 推导 WebCodecs 的 codec 字符串，例如 `"avc1.64001F"`。
///
/// 只取 NAL 头之后的三个字节：profile_idc / constraint_flags / level_idc。
/// 这三个字节错了没法靠猜补救，缺 SPS 时返回 None 让前端退回到公开的
/// Baseline 字符串。
fn codec_string_from_sps(sps: &[u8]) -> Option<String> {
    let offset = if sps.starts_with(&SC4) {
        4
    } else if sps.starts_with(&SC3) {
        3
    } else {
        return None;
    };
    let bytes = sps.get(offset + 1..offset + 4)?;
    Some(format!(
        "avc1.{:02X}{:02X}{:02X}",
        bytes[0], bytes[1], bytes[2]
    ))
}

/// 从 Annex-B 字节里找出第一段 SPS（type 7）和 SPS 之后的第一段 PPS（type 8）。
///
/// 直接吃原始字节而不是 NAL 列表：这两个 NAL 在流的最开头，而那时缓冲区里的
/// 参数集还没来得及被切成 NAL 发出去。
fn extract_parameter_sets(bytes: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let codes = find_all_start_codes(bytes);
    let mut sps = None;
    let mut pps = None;
    let mut seen_sps = false;

    for (index, code) in codes.iter().enumerate() {
        let end = codes.get(index + 1).map_or(bytes.len(), |next| next.pos);
        let header = bytes.get(code.pos + code.sc_len).copied().unwrap_or(0);
        match header & 0x1f {
            7 => {
                seen_sps = true;
                if sps.is_none() {
                    sps = Some(bytes[code.pos..end].to_vec());
                }
            }
            // PPS 必须在 SPS 之后才是同一个序列。
            8 if seen_sps => {
                pps = Some(bytes[code.pos..end].to_vec());
                break;
            }
            _ => {}
        }
    }
    (sps, pps)
}

/// 一帧的裸数据，以及它在时间轴上对应的位置。
struct VideoPacket {
    data: Vec<u8>,
    timestamp: f64,
    duration: f64,
    key: bool,
}

/// 发给前端的每帧元数据。
///
/// 长成这个样子是因为 `VideoFrame`/`EncodedVideoChunk` 需要每帧的时间戳与
/// 关键帧标记，而 WebCodecs 自己不做 Annex-B 拆帧，这些信息只能在 Rust 侧
/// 算好后随帧传过去。
#[derive(Serialize)]
struct Sample {
    /// 显示时间戳（微秒）
    timestamp: f64,
    /// 时长（微秒）
    duration: f64,
    /// 是否关键帧（含 IDR slice）
    key: bool,
    /// Annex-B 格式的访问单元数据
    data: Vec<u8>,
}

/// 一个待发送的成批消息：把若干帧塞进同一条 IPC。
#[derive(Serialize)]
struct SamplesPayload {
    #[serde(rename = "type")]
    kind: &'static str,
    samples: Vec<Sample>,
}

/// 解码器初始化信息。发在第一帧之前，前端据此 configure。
///
/// 字段要走 camelCase，和前端 `ConfigMessage` 的 `descriptionLen` 对齐——
/// 少了 rename_all 会让前端读到 undefined（日志里会显示"参数集 undefined 字节"）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigPayload {
    #[serde(rename = "type")]
    kind: &'static str,
    codec: String,
    width: u32,
    height: u32,
    /// 访问单元里带内的参数集（SPS/PPS 的拼接）字节数，便于前端排查
    description_len: usize,
}

/// 把一帧编码成 Rust 侧的输入。
fn to_sample(packet: VideoPacket) -> Sample {
    Sample {
        timestamp: packet.timestamp,
        duration: packet.duration,
        key: packet.key,
        data: packet.data,
    }
}

/// 把若干帧拼成一条 IPC 消息的 JSON 文本。
///
/// 每个 AU 的元数据（时间戳/时长/关键帧）必须和它的裸数据一起送过去，
/// 它们没法像纯字节流那样拼接，所以这里显式编码成 JSON。
fn encode_samples(samples: Vec<Sample>) -> String {
    let payload = SamplesPayload {
        kind: "samples",
        samples,
    };
    serde_json::to_string(&payload).unwrap_or_else(|e| {
        // serde 只在这几个基本类型上失败才可能走到这里，真出错也不能把
        // 整个流水线静默掉。
        format!("{{\"type\":\"error\",\"message\":{:?}}}", e.to_string())
    })
}

fn encode_config(codec: &str, width: u32, height: u32, description_len: usize) -> String {
    let payload = ConfigPayload {
        kind: "config",
        codec: codec.to_string(),
        width,
        height,
        description_len,
    };
    serde_json::to_string(&payload).unwrap_or_else(|e| {
        format!("{{\"type\":\"error\",\"message\":{:?}}}", e.to_string())
    })
}

/// 流结束通知。前端收到后把队列里剩下的帧按时间戳播完再停。
#[derive(Serialize)]
struct EndPayload {
    #[serde(rename = "type")]
    kind: &'static str,
}

fn encode_end() -> String {
    serde_json::to_string(&EndPayload { kind: "end" })
        .unwrap_or_else(|_| "{\"type\":\"end\"}".to_string())
}

/// 把 JSON 文本转成 IPC 原始字节。
///
/// 必须过这一道：`Response::new(String)` 会被当成 `InvokeResponseBody::Json`，
/// 而 `Vec<u8>` 才是清晰无歧义的 Raw 分支。前端只需要按 UTF-8 解码再解析 JSON，
/// 不用去猜有没有被多包一层引号。
fn json_bytes(payload: String) -> Vec<u8> {
    payload.into_bytes()
}

/// 发一批数据。发送失败不再被静默忽略，错误原样交给上层。
///
/// 刻意不打日志：这里每批都会走到，是一条几分钟的视频要跑几百上千次的热路径，
/// 逐批 println! 是同步 I/O，会把整个流水线拖慢。批数由调用方在结束时汇总打印。
fn send_batch<F>(batch: &mut Vec<Sample>, flush: &mut F) -> Result<(), VideoError>
where
    F: FnMut(String) -> Result<(), VideoError>,
{
    if batch.is_empty() {
        return Ok(());
    }
    let samples = std::mem::take(batch);
    let payload = encode_samples(samples);
    flush(payload)
}

/// 把 AU 按 [`BATCH_TARGET`] 聚合成若干批 JSON 消息发出去，返回实际发出的批数。
///
/// 这里按"帧"而非"字节"判断上限，因为每帧都带着时间戳/关键帧元数据，
/// 一条消息里能装的帧数才是决定性的；帧本身的大小差异很大（IDR 可能几 MB）。
fn flush_packets<F>(packets: Vec<VideoPacket>, flush: &mut F) -> Result<usize, VideoError>
where
    F: FnMut(String) -> Result<(), VideoError>,
{
    let mut batch: Vec<Sample> = Vec::new();
    let mut batch_bytes = 0usize;
    let mut batches = 0usize;

    for packet in packets {
        let packet_bytes = packet.data.len();
        if !batch.is_empty() && batch_bytes + packet_bytes > BATCH_TARGET {
            send_batch(&mut batch, flush)?;
            batches += 1;
            batch_bytes = 0;
        }
        batch_bytes += packet_bytes;
        batch.push(to_sample(packet));
    }

    if !batch.is_empty() {
        send_batch(&mut batch, flush)?;
        batches += 1;
    }
    Ok(batches)
}

async fn ffmpeg_h264(
    video_path: &str,
    on_chunk: Channel<Response>,
    info: &MediaInfo,
) -> Result<(), VideoError> {
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

    // 未收齐的字节。最后一个 AU 的原始字节也放回这里，和新数据一起重新切。
    let mut pending_bytes: Vec<u8> = Vec::new();
    let mut read_buf = [0u8; 64 * 1024];
    let mut line_string = String::new();

    // 没有 B 帧时 DTS == PTS，按固定帧长递增时间戳就足够；
    // 有 B 帧的流需要按 POC 重排序，那是下一步的事。
    let frame_duration = info.frame_duration_micros();
    let start_timestamp = 0.0f64;
    let mut sent_frames = 0usize;
    let mut sent_batches = 0usize;
    let mut config_sent = false;
    // 诊断提示只打一次。
    let mut diag_note_printed = false;

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
                        pending_bytes.extend_from_slice(&read_buf[..n]);

                        // 配置消息要在参数集被消费掉之前发出去，所以先在这里取。
                        if !config_sent {
                            let (sps, pps) = extract_parameter_sets(&pending_bytes);
                            let codec = sps
                                .as_deref()
                                .and_then(codec_string_from_sps)
                                .unwrap_or_else(|| "avc1.42E01E".to_string());
                            let description_len = sps
                                .as_ref()
                                .map_or(0, |v| v.len())
                                + pps.as_ref().map_or(0, |v| v.len());
                            println!(
                                "[video] 解码器配置: codec={codec}, {}x{}, 参数集 {description_len} 字节",
                                info.width.unwrap_or(0),
                                info.height.unwrap_or(0),
                            );
                            if sps.is_none() {
                                eprintln!("警告：这一批里没找到 SPS，codec 只能退回默认值");
                            }
                            // 一定要走 Vec<u8>（Raw 分支）：String 会变成
                            // InvokeResponseBody::Json，到 JS 端可能被多包一层引号，
                            // 前端就得先剥引号再解析。Raw 没有这层歧义。
                            on_chunk
                                .send(Response::new(json_bytes(encode_config(
                                    &codec,
                                    info.width.unwrap_or(0),
                                    info.height.unwrap_or(0),
                                    description_len,
                                ))))
                                .map_err(VideoError::TauriError)?;
                            config_sent = true;
                        }

                        // 一帧还没收齐时缓冲会一直长；正常到不了这个量级，
                        // 真到了说明流里有异常大的帧，报出来比静默吃内存好。
                        if pending_bytes.len() > MAX_PENDING_BYTES {
                            eprintln!(
                                "警告：组帧缓冲已到 {} 字节，仍未见完整帧边界",
                                pending_bytes.len()
                            );
                        }

                        let (fresh_nals, carry) = parse_nals(&mut pending_bytes);
                        // 这批 NAL 里第一个 AU 的时间戳：按已经发出去的帧数推。
                        // 不能用"每批加一个帧长"——一批里可能有多个 AU。
                        let batch_timestamp = start_timestamp + sent_frames as f64 * frame_duration;
                        let (packets, pending_carry) =
                            group_access_units(fresh_nals, batch_timestamp, frame_duration);

                        // 诊断：看每个 AU 的 NAL 类型组成与关键帧标记。
                        // 默认只打前 DIAG_FRAMES 个，NAL_DEBUG=all 打全部。
                        for (offset, packet) in packets.iter().enumerate() {
                            trace_access_unit(sent_frames + offset, packet);
                        }
                        trace_truncated_note(sent_frames + packets.len(), &mut diag_note_printed);

                        sent_frames += packets.len();
                        // 闭包在调用点直接构造：on_chunk 是 FnMut 持有者，
                        // 存成具名变量会和外层其他借用撞车。
                        sent_batches += flush_packets(packets, &mut |payload: String| {
                            on_chunk
                                .send(Response::new(json_bytes(payload)))
                                .map_err(VideoError::TauriError)
                        })?;

                        // 未成帧的尾部 NAL 字节 + 解析时保留的未定界尾巴，
                        // 下一轮和新读到的数据一起重新切。两者都只包含
                        // "还没被发出去过"的字节，不会重复。
                        pending_bytes = pending_carry;
                        pending_bytes.extend_from_slice(&carry);
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

    // 统一的发送入口：所有消息都转成 Raw 字节再发，失败不会被静默吞掉。
    let mut send = |payload: String| -> Result<(), VideoError> {
        on_chunk
            .send(Response::new(json_bytes(payload)))
            .map_err(VideoError::TauriError)
    };

    // 收尾：流结束后待处理的字节要全部成帧发出去，否则整段视频会丢掉尾部一帧。
    //
    // 补两个起始码：第一个让最后那段数据成为完整 NAL，第二个让它不再是"还没有
    // 后续起始码来定界"的尾巴，这样 group_access_units 才会把它当完整 AU 发出。
    if !pending_bytes.is_empty() {
        if find_all_start_codes(&pending_bytes).is_empty() {
            eprintln!("流结束后仍有多余字节但找不到起始码，已丢弃 {} 字节", pending_bytes.len());
        } else {
            let mut tail_bytes = std::mem::take(&mut pending_bytes);
            tail_bytes.extend_from_slice(&SC4);
            tail_bytes.extend_from_slice(&SC4);
            let (tail_nals, _) = parse_nals(&mut tail_bytes);

            let tail_timestamp = start_timestamp + sent_frames as f64 * frame_duration;
            let (packets, _) = group_access_units(tail_nals, tail_timestamp, frame_duration);
            if !packets.is_empty() {
                sent_frames += packets.len();
                sent_batches += flush_packets(packets, &mut send)?;
            }
        }
    }

    // 前端靠这条消息才知道"不会再有帧了"：播放时钟要据此把队列里剩下的帧
    // 按各自时间戳放完，然后停下来。
    send(encode_end())?;

    println!(
        "ffmpeg_h264 done, sent {sent_frames} frames in {sent_batches} batches, exit: {status}"
    );

    Ok(())
}

/// 把残留的 NAL 重新拼回 Annex-B 字节流（只在测试里用来构造期望值）。
///
/// 这些 NAL 自带起始码，直接首尾相接即可（起始码本身就是分隔符）。
#[cfg(test)]
fn join_nals(nals: &[VideoNal]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nals.iter().map(|n| n.data.len()).sum());
    for nal in nals {
        out.extend_from_slice(&nal.data);
    }
    out
}

/// 校验文件存在并读取基本信息，作为播放 / 处理前的准备步骤。
pub async fn start(video_path: &str, on_chunk: Channel<Response>) -> Result<MediaInfo, VideoError> {
    let path = Path::new(video_path);
    if !path.is_file() {
        return Err(VideoError::FileNotFound(video_path.to_string()));
    }

    let info = probe(path).await?;
    println!(
        "[video] {} -> {:.2}s, {} 字节, 容器 {}, 视频 {}, 音频 {}, {}x{}, {:.3}fps",
        video_path,
        info.duration,
        info.size,
        info.format,
        info.video_codec.as_deref().unwrap_or("无"),
        info.audio_codec.as_deref().unwrap_or("无"),
        info.width.unwrap_or(0),
        info.height.unwrap_or(0),
        info.frame_rate.unwrap_or(30.0),
    );

    ffmpeg_h264(video_path, on_chunk, &info).await?;

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
        // 只有一个起始码：还不能确定任何 NAL 的结束位置，整段留到下一轮。
        let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA];
        let (nals, carry) = parse_nals(&mut buf);
        assert!(nals.is_empty());
        assert_eq!(carry, vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA]);
        assert!(buf.is_empty(), "留下的字节必须同时从缓冲区里移走，避免重复");
    }

    #[test]
    fn parse_splits_nals_and_keeps_last() {
        // 两个完整 NALU 边界 + 一个还没定界的尾巴。
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

        let (nals, carry) = parse_nals(&mut buf);

        let mut expected_first = SC4.to_vec();
        expected_first.extend_from_slice(&payload_a);
        let mut expected_second = SC3.to_vec();
        expected_second.extend_from_slice(&payload_b);
        let datas: Vec<Vec<u8>> = nals.iter().map(|n| n.data.clone()).collect();
        assert_eq!(datas, vec![expected_first, expected_second]);
        // 头字节里的两个字段必须一起解析出来，下游判断随机访问点全靠它们。
        assert_eq!(nals[0].nal_type, 7);
        assert_eq!(nals[1].nal_type, 8);

        let mut expected_tail = SC4.to_vec();
        expected_tail.extend_from_slice(&tail);
        assert_eq!(carry, expected_tail);
    }

    #[test]
    fn parse_reassembles_split_start_code_across_reads() {
        // 起始码被切在两次读取之间：第一块以起始码 + 头字节 + 半个载荷结尾，
        // 第二块补上 00 00 01 和新的头字节。跨块拼接后必须能识别出来。
        //
        // 注意这里刻意不写 [.., 0x67, 0xAA, 0x00] 这种"末尾补一个 0"的形式：
        // 那个 0 会和后面的 00 00 01 连成 00 00 00 01，被正确地识别成一个
        // 4 字节起始码（规范允许），于是数据被分成两段，反而看不清要测的东西。
        let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA];
        let (nals, carry) = parse_nals(&mut buf);
        assert!(nals.is_empty(), "只有一个起始码，还切不出任何 NAL");
        assert_eq!(carry, vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA]);
        assert!(buf.is_empty(), "留下的字节必须同时从缓冲区里移走，避免重复");

        // 把 carry 接回缓冲区（模拟主循环的做法），再补上新数据。
        buf = carry;
        buf.extend_from_slice(&[0x00, 0x00, 0x01, 0x68]);
        let (nals, carry) = parse_nals(&mut buf);
        // 现在能切出一个完整的 NAL，且它的头字节是 0x67。
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].data, vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA]);
        assert_eq!(nals[0].nal_type, 7);
        assert_eq!(carry, vec![0x00, 0x00, 0x01, 0x68]);
    }

    /// 构造一个 NAL：4 字节起始码 + 头字节 + 载荷。
    ///
    /// `nal_ref_idc` 默认取 3（参与参考），因为绝大多数实际 NAL 都是这样；
    /// 需要测非参考的情况单独用 `nal_with_idc`。
    fn nal(nal_type: u8, payload: &[u8]) -> Vec<u8> {
        nal_with_idc(nal_type, 3, payload)
    }

    fn nal_with_idc(nal_type: u8, ref_idc: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = SC4.to_vec();
        out.push(((ref_idc & 0x03) << 5) | (nal_type & 0x1f));
        out.extend_from_slice(payload);
        out
    }

    /// 把裸字节包成解析后的 NAL（测试里直接用）。
    fn vnal(bytes: Vec<u8>) -> VideoNal {
        let header = bytes.get(4).copied().unwrap_or(0);
        VideoNal {
            nal_type: header & 0x1f,
            ref_idc: (header >> 5) & 0x03,
            data: bytes,
        }
    }

    /// 构造一个 I slice（type 1，非 IDR）：`first_mb_in_slice=0`、`slice_type=2`。
    ///
    /// 两个 `ue(v)` 在 RBSP 里首尾相接：
    /// - `first_mb_in_slice = 0` → 码字 `1`
    /// - `slice_type = 2` → 码字 `011`（1 个前导零 + 信息位 `1`）
    ///
    /// 连起来是 `1 011 xxxx`，所以第一个字节必须是 `0xB0`。
    /// （写成 `0xE0` 是 `1 1 ...`，第二个 ue 解出来是 0，即 **P** slice——
    ///  这个坑真的踩过一次，测试数据比被测代码更容易写错。）
    fn i_slice_nal(ref_idc: u8) -> Vec<u8> {
        nal_with_idc(1, ref_idc, &[0xB0, 0x00, 0x00])
    }

    /// 构造一个 P slice（type 1，非 IDR）：`first_mb_in_slice=0`、`slice_type=0`。
    ///
    /// `slice_type = 0` → 码字 `1`；连起来是 `1 1 xxxxxx` → `0xC0`。
    fn p_slice_nal() -> Vec<u8> {
        nal(1, &[0xC0, 0x00])
    }

    /// 把已经切好的 NAL 字节直接喂给聚合器，返回 (帧列表, 残留字节)。
    ///
    /// 注意这里绕开了 `parse_nals`：适合构造"确定完整的 NAL"来测聚合逻辑。
    /// 需要测"截断的尾巴"时要走 `parse_nals`，因为只有它能判断完整性。
    fn group_raw(raw: &[Vec<u8>], start: f64, duration: f64) -> (Vec<VideoPacket>, Vec<u8>) {
        group_access_units(raw.iter().cloned().map(vnal).collect(), start, duration)
    }

    #[test]
    fn groups_parameter_sets_with_idr_into_one_frame() {
        // SPS + PPS + SEI + IDR slice + 下一个 slice：第一个 AU 到这里就完整了，
        // 后面那个 slice 属于下一帧，作为残留字节留下。
        let raw = vec![
            nal(7, &[0x64, 0x00, 0x1f]),
            nal(8, &[0xAA]),
            nal(6, &[0xBB]),
            nal(5, &[0xCC]),
            p_slice_nal(),
        ];
        let (units, carry) = group_raw(&raw, 0.0, 33_333.0);

        assert_eq!(units.len(), 1);
        assert!(units[0].key, "含 IDR 的 AU 必须标记为关键帧");
        // 第一帧必须完整包含 SPS/PPS/SEI/IDR 四个 NAL，一个都不能少。
        assert_eq!(units[0].data, join_nals(&raw[..4].iter().cloned().map(vnal).collect::<Vec<_>>()));
        assert_eq!(carry, raw[4]);
    }

    #[test]
    fn parameter_sets_are_never_split_into_their_own_frame() {
        // 真实踩到的坑：ffmpeg 会在关键帧前输出 SPS+PPS，如果这时候就把它们
        // 切成一帧发出去，就是一个"标记为 key 但不含图像数据"的 chunk，
        // 解码器收到后会立刻进入 "A key frame is required" 状态，之后全解不出来。
        //
        // 末尾必须再给一个 slice：设计上最后一个 AU 永不发出（它的结束边界还没
        // 确定），需要有下一个 NAL 来界定被检查的这一帧。
        let raw = vec![nal(7, &[0x64, 0x00, 0x1f]), nal(8, &[0xAA]), i_slice_nal(3), p_slice_nal()];
        let (units, carry) = group_raw(&raw, 0.0, 33_333.0);

        assert_eq!(units.len(), 1, "SPS+PPS+I slice 必须合成一帧，不能拆出参数集帧");
        assert!(units[0].key, "含 I slice 的帧必须标记为关键帧");
        // SPS / PPS / I slice 三个 NAL 必须都在这一帧里，一个都不能少。
        assert_eq!(units[0].data, join_nals(&raw[..3].iter().cloned().map(vnal).collect::<Vec<_>>()));
        // 绝不能有任何一帧是空的——空帧同样会让解码器卡在"需要关键帧"。
        assert!(units.iter().all(|u| !u.data.is_empty()), "绝不能发出空帧");
        assert_eq!(carry, raw[3], "末尾那个 slice 留到下一轮");
    }

    #[test]
    fn idr_slice_is_marked_key_even_when_it_follows_parameter_sets() {
        // SPS + PPS 先到、IDR 后到（跨批）时，key 标记必须落在带 slice 的那一帧上，
        // 而不是在参数集那一批就"用掉"。
        let (units_a, carry) = group_raw(&[nal(7, &[0x64, 0x00, 0x1f]), nal(8, &[0xAA])], 0.0, 1.0);
        assert!(units_a.is_empty(), "只有参数集时不能出帧");
        assert!(!carry.is_empty());

        let mut bytes = carry;
        bytes.extend_from_slice(&nal(5, &[0xCC]));
        bytes.extend_from_slice(&p_slice_nal());
        bytes.extend_from_slice(&p_slice_nal());
        let (nals, _) = parse_nals(&mut bytes);
        let (units_b, _) = group_access_units(nals, 0.0, 1.0);

        // 末尾那个 slice 会被作为未定界的尾巴留下，所以这里切出 1 帧。
        assert_eq!(units_b.len(), 1);
        assert!(units_b[0].key);
        assert!(units_b[0].data.starts_with(&nal(7, &[0x64, 0x00, 0x1f])));
    }

    #[test]
    fn non_idr_i_slice_counts_as_key_frame() {
        // 这是真实踩到的坑：Main profile 的流用非 IDR 的 I 帧开头（NAL 类型是 1
        // 而不是 5）。只看 type 5 会把这种真正的关键帧判成 delta，解码器立刻报
        // "A key frame is required after configure()"。
        let raw = vec![nal(7, &[0x64, 0x00, 0x1f]), i_slice_nal(3), p_slice_nal()];
        let (units, _) = group_raw(&raw, 0.0, 33_333.0);

        assert_eq!(units.len(), 1);
        assert!(units[0].key, "非 IDR 的 I slice 同样是合法的解码起点");
    }

    #[test]
    fn non_reference_i_slice_is_not_marked_key() {
        // nal_ref_idc == 0 的 I slice 不参与参考，不能当成可靠的随机访问点。
        let raw = vec![i_slice_nal(0), p_slice_nal()];
        let (units, _) = group_raw(&raw, 0.0, 1.0);

        assert_eq!(units.len(), 1);
        assert!(!units[0].key);
    }

    #[test]
    fn p_slices_are_not_key_frames() {
        let raw = vec![p_slice_nal(), p_slice_nal()];
        let (units, _) = group_raw(&raw, 0.0, 1.0);

        assert_eq!(units.len(), 1);
        assert!(!units[0].key, "P slice 不能被当成关键帧");
    }

    #[test]
    fn groups_two_p_frames_into_two_frames() {
        // 三个 P slice：前两个各自成为一帧，最后那个的边界还没确定，留作残留。
        let raw = vec![p_slice_nal(), p_slice_nal(), p_slice_nal()];
        let (units, carry) = group_raw(&raw, 0.0, 33_333.0);

        assert_eq!(units.len(), 2);
        assert!(!units[0].key);
        assert!(!units[1].key);
        assert_eq!(carry, raw[2], "队列末尾那个 NAL 不能参与成帧");
    }

    #[test]
    fn incomplete_trailing_nal_is_never_emitted_as_a_frame() {
        // 最关键的回归测试：read 边界落在帧中间时，最后一个 NAL 是截断的。
        // 它的长度只能由"后续起始码"确定，所以绝不能当成完整帧发出去。
        //
        // 注意必须走 `parse_nals` 来构造：只有它会判断"这段数据够不够完整"。
        // 直接手搓一个短 NAL 交给聚合器是无效的——聚合器拿到的是"已经确认完整"
        // 的 NAL，它不会（也没法）再判断完整性。
        let mut bytes = p_slice_nal();
        bytes.extend_from_slice(&p_slice_nal());
        bytes.extend_from_slice(&p_slice_nal()[..6]); // 截断：只有起始码 + 头字节

        let (nals, carry) = parse_nals(&mut bytes);
        // 三个起始码 → 两个窗口（0..7、7..14），所以是两个完整 NAL：
        // 第一个 P slice 的结束由第二个的起始码界定，第二个由第三个界定。
        assert_eq!(nals.len(), 2, "只有被下一个起始码界定的才算完整 NAL");
        assert_eq!(nals[0].data, p_slice_nal());
        assert_eq!(nals[1].data, p_slice_nal());
        // 截断的那个必须原样留在 carry 里，绝不能变成第 3 个"完整 NAL"。
        assert_eq!(carry, p_slice_nal()[..6], "截断的 NAL 必须原样保留在 carry 里");

        let (units, _) = group_access_units(nals, 0.0, 33_333.0);
        // 两个完整 NAL = 前一个成帧、后一个等边界，所以这里出 1 帧。
        assert_eq!(units.len(), 1);
        // 被切断的 NAL 绝不能出现在任何已发送的帧里。
        assert_eq!(units[0].data, p_slice_nal(), "帧里只能有确定完整的那一个 slice");
        assert!(
            !units[0].data.ends_with(&p_slice_nal()[..6]),
            "被切断的 NAL 绝不能进入已发送的帧"
        );
    }

    #[test]
    fn carry_can_be_appended_and_reparsed() {
        // 残留字节接上新数据后必须能被重新切开，不能错位。
        //
        // 全程走字节层（parse_nals），和主循环的做法一致：carry 是"还没定界的
        // 原始字节"，只能接回缓冲区再解析一次，不能直接当成 NAL 用。
        let mut bytes = p_slice_nal();
        bytes.extend_from_slice(&p_slice_nal());
        let (nals, carry) = parse_nals(&mut bytes);
        assert_eq!(nals.len(), 1, "第二个 slice 的结束边界还没到");
        assert_eq!(carry, p_slice_nal(), "carry 是原始字节，不是 NAL");

        let (units_a, _) = group_access_units(nals, 0.0, 1000.0);
        assert_eq!(units_a.len(), 0, "只有一个 NAL，成不了帧（要等下一个边界）");

        // 接上两个新的 slice：carry 里那个 NAL 现在有"下一个起始码"来定界了。
        let mut bytes = carry;
        bytes.extend_from_slice(&p_slice_nal());
        bytes.extend_from_slice(&p_slice_nal());
        let (nals, carry) = parse_nals(&mut bytes);
        assert_eq!(nals.len(), 2, "carry 里那个 + 新加的第一个都完整了");
        assert_eq!(carry, p_slice_nal());

        let (units_b, _) = group_access_units(nals, 1000.0, 1000.0);
        assert_eq!(units_b.len(), 1, "两帧之间需要一个边界来界定前一个");
        assert_eq!(units_b[0].timestamp, 1000.0);
        assert_eq!(units_b[0].data, p_slice_nal());
    }

    #[test]
    fn parameters_before_next_frame_start_a_new_frame() {
        // P slice 之后又出现 SPS/PPS/IDR：这是新的 AU（例如场景切换后的 IDR 帧）。
        let raw = vec![
            p_slice_nal(),
            nal(7, &[0x64, 0x00, 0x1f]),
            nal(8, &[0xAA]),
            nal(5, &[0xCC]),
            p_slice_nal(),
        ];
        let (units, _) = group_raw(&raw, 0.0, 33_333.0);

        assert_eq!(units.len(), 2);
        assert!(!units[0].key);
        assert!(units[1].key);
    }

    #[test]
    fn only_parameter_sets_means_nothing_is_emitted_yet() {
        // 只读到 SPS/PPS：还没有图像数据，绝不能当成完整帧发出去；
        // 这两个 NAL 的字节整体留到下一轮。
        let raw = vec![nal(7, &[0x64, 0x00, 0x1f]), nal(8, &[0xAA])];
        let (units, carry) = group_raw(&raw, 0.0, 33_333.0);

        assert!(units.is_empty());
        assert_eq!(carry, join_nals(&raw.iter().cloned().map(vnal).collect::<Vec<_>>()));
    }

    #[test]
    fn each_frame_in_a_batch_gets_its_own_timestamp() {
        // 同一次 read 里出现多个 AU 时，它们的时间戳必须依次递增，
        // 否则前端会拿到重复时间戳（这正是之前画面卡住的原因之一）。
        let raw = vec![p_slice_nal(), p_slice_nal(), p_slice_nal(), p_slice_nal()];
        let (units, _) = group_raw(&raw, 1000.0, 40.0);

        assert_eq!(units.len(), 3);
        assert_eq!(units[0].timestamp, 1000.0);
        assert_eq!(units[1].timestamp, 1040.0);
        assert_eq!(units[2].timestamp, 1080.0);
        for unit in &units {
            assert_eq!(unit.duration, 40.0);
        }
    }

    #[test]
    fn diagnostic_line_reports_nal_types_and_key_flag() {
        // 诊断输出是排查 "参数集被单独成帧""关键帧被漏标" 的第一手线索，
        // 格式必须稳定：类型序列 + key 标记。
        let raw = vec![nal(7, &[0x64, 0x00, 0x1f]), nal(8, &[0xAA]), nal(5, &[0xCC]), p_slice_nal()];
        let (units, _) = group_raw(&raw, 0.0, 33_333.0);

        assert_eq!(units.len(), 1);
        // 第一帧的 NAL 类型序列应当是 SPS、PPS、IDR。
        assert_eq!(collect_nal_types(&units[0].data), vec![7, 8, 5]);
        assert_eq!(describe_access_unit(&[7, 8, 5], true), "[7,8,5] key=true");
        assert_eq!(describe_access_unit(&[1], false), "[1] key=false");
    }

    #[test]
    fn diag_level_defaults_to_first_frames() {
        // 默认档必须是"打前几帧"——全量打印会拖慢流水线，而完全不打又失去了
        // 排查能力。这里只能验证档位枚举本身，环境变量读取不参与断言。
        assert_ne!(Diag::FirstFrames, Diag::Off);
        assert_ne!(Diag::FirstFrames, Diag::All);
    }

    #[test]
    fn parses_i_slice_slice_type_from_rbsp() {        // slice_type 解析必须正确区分 I 和 P，这是关键帧判定的基础。
        assert_eq!(parse_slice_type(&i_slice_nal(3)), Some(2));
        assert_eq!(parse_slice_type(&p_slice_nal()), Some(0));

        // slice_type 7（I 的另一种写法）模 5 之后同样是 2：
        // 码字 `0001000` + 信息位 `011` = `0001000011`，即 `0x10 0xC0`。
        let i_slice_alt = nal_with_idc(1, 3, &[0x10, 0xC0]);
        assert_eq!(parse_slice_type(&i_slice_alt), Some(2));
    }

    #[test]
    fn rbsp_reader_skips_emulation_prevention_bytes() {
        // 00 00 03 xx 里的 03 是编码时插进来的，读取时必须跳过，
        // 否则 slice header 的比特位会整体错位。
        let payload = [0x00, 0x00, 0x03, 0x01];
        assert_eq!(rbsp_byte(&payload, 0), Some(0x00));
        assert_eq!(rbsp_byte(&payload, 1), Some(0x00));
        // 第三个 RBSP 字节是 0x01，而不是防竞争字节 0x03。
        assert_eq!(rbsp_byte(&payload, 2), Some(0x01));
    }

    #[test]
    fn codec_string_comes_from_sps_bytes() {
        // 必须带上起始码：真实的 SPS NAL 就是"起始码 + 头字节 + 数据"。
        // 真实 Main profile SPS：67 4D 40 1E ... → profile 0x4D, constraints 0x40, level 0x1E
        let sps = nal(7, &[0x4D, 0x40, 0x1e, 0xac, 0xd9, 0x40]);
        assert_eq!(codec_string_from_sps(&sps).as_deref(), Some("avc1.4D401E"));

        // High profile 的例子（0x64 / 0x00 / 0x1F）。
        let sps = nal(7, &[0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40]);
        assert_eq!(codec_string_from_sps(&sps).as_deref(), Some("avc1.64001F"));

        // 拿不到完整字节时不能瞎猜，让调用方退回默认字符串。
        assert_eq!(codec_string_from_sps(&[0x67, 0x64]), None);
        assert_eq!(codec_string_from_sps(&[]), None);
    }

    #[test]
    fn finds_sps_and_pps_in_order() {
        let raw_sps = nal(7, &[0x64, 0x00, 0x1f]);
        let raw_pps = nal(8, &[0xAA]);

        // 直接喂原始字节：参数集要在被切成 NAL 之前就取出来。
        let mut bytes = raw_sps.clone();
        bytes.extend_from_slice(&raw_pps);
        let (found_sps, found_pps) = extract_parameter_sets(&bytes);
        assert_eq!(found_sps.as_deref(), Some(raw_sps.as_slice()));
        assert_eq!(found_pps.as_deref(), Some(raw_pps.as_slice()));

        // 只有 SPS、没有 PPS 时不能硬凑。
        let (only_sps, none) = extract_parameter_sets(&raw_sps);
        assert!(only_sps.is_some());
        assert!(none.is_none());

        // 空输入不能 panic。
        let (empty_sps, empty_pps) = extract_parameter_sets(&[]);
        assert!(empty_sps.is_none() && empty_pps.is_none());
    }

    #[test]
    fn frame_rate_parses_rational_strings() {
        let stream = serde_json::json!({"avg_frame_rate": "30000/1001", "r_frame_rate": "30/1"});
        let fps = parse_frame_rate(&stream).unwrap();
        assert!((fps - 29.97).abs() < 0.01);

        // avg 为 0/0 时退回 r_frame_rate。
        let stream = serde_json::json!({"avg_frame_rate": "0/0", "r_frame_rate": "25/1"});
        assert_eq!(parse_frame_rate(&stream), Some(25.0));

        // 两个都没有 / 都非法时返回 None，由调用方退回默认帧率。
        assert_eq!(parse_frame_rate(&serde_json::json!({})), None);
        assert_eq!(parse_frame_rate(&serde_json::json!({"r_frame_rate": "x/y"})), None);
    }

    #[test]
    fn frame_duration_falls_back_and_never_divides_by_zero() {
        let mut info = MediaInfo {
            duration: 1.0,
            size: 0,
            format: String::new(),
            video_codec: None,
            audio_codec: None,
            width: None,
            height: None,
            frame_rate: Some(25.0),
        };
        assert_eq!(info.frame_duration_micros(), 40_000.0);

        info.frame_rate = None;
        assert_eq!(info.frame_duration_micros(), 1_000_000.0 / 30.0);

        // 0 帧率不能除出无穷大。
        info.frame_rate = Some(0.0);
        assert_eq!(info.frame_duration_micros(), 1_000_000.0);
    }

    #[test]
    fn batches_small_packets_together() {
        // 两个小帧会被合并成一批，消息数从 2 降到 1。
        let packets = vec![
            VideoPacket { data: vec![1u8; 10], timestamp: 0.0, duration: 1.0, key: true },
            VideoPacket { data: vec![2u8; 20], timestamp: 1.0, duration: 1.0, key: false },
        ];
        let mut sent: Vec<String> = Vec::new();
        let batches = {
            let mut flush = |payload: String| -> Result<(), VideoError> {
                sent.push(payload);
                Ok(())
            };
            flush_packets(packets, &mut flush).unwrap()
        };

        assert_eq!(batches, 1);
        assert_eq!(sent.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(value["type"], "samples");
        assert_eq!(value["samples"].as_array().unwrap().len(), 2);
        assert_eq!(value["samples"][0]["key"], true);
        // 每帧的 data 是 JSON 数字数组，长度必须原样保留。
        assert_eq!(value["samples"][0]["data"].as_array().unwrap().len(), 10);
        assert_eq!(value["samples"][1]["data"].as_array().unwrap().len(), 20);
    }

    #[test]
    fn batching_splits_at_target_and_never_truncates() {
        // 三帧略小于目标的数据，应当分成两批；总帧数必须与输入一致。
        let each = BATCH_TARGET / 3 + 1;
        let packets = vec![
            VideoPacket { data: vec![1u8; each], timestamp: 0.0, duration: 1.0, key: false },
            VideoPacket { data: vec![2u8; each], timestamp: 1.0, duration: 1.0, key: false },
            VideoPacket { data: vec![3u8; each], timestamp: 2.0, duration: 1.0, key: false },
        ];
        let mut sent: Vec<String> = Vec::new();
        let batches = {
            let mut flush = |payload: String| -> Result<(), VideoError> {
                sent.push(payload);
                Ok(())
            };
            flush_packets(packets, &mut flush).unwrap()
        };

        assert!(batches >= 2);
        let total: usize = sent
            .iter()
            .map(|s| {
                let v: serde_json::Value = serde_json::from_str(s).unwrap();
                v["samples"].as_array().unwrap().len()
            })
            .sum();
        assert_eq!(total, 3, "拆批不能丢帧");
    }

    #[test]
    fn oversized_single_frame_is_sent_alone() {
        // 单个超过目标的 AU（IDR 帧很常见）必须单独一条发，不能被丢弃。
        let big = BATCH_TARGET + 1024;
        let packets = vec![
            VideoPacket { data: vec![7u8; big], timestamp: 0.0, duration: 1.0, key: true },
            VideoPacket { data: vec![8u8; 16], timestamp: 1.0, duration: 1.0, key: false },
        ];
        let mut sent: Vec<String> = Vec::new();
        let batches = {
            let mut flush = |payload: String| -> Result<(), VideoError> {
                sent.push(payload);
                Ok(())
            };
            flush_packets(packets, &mut flush).unwrap()
        };

        assert_eq!(batches, 2);
        let first: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(first["samples"][0]["data"].as_array().unwrap().len(), big);
        let second: serde_json::Value = serde_json::from_str(&sent[1]).unwrap();
        assert_eq!(second["samples"][0]["data"].as_array().unwrap().len(), 16);
    }

    #[test]
    fn carry_jams_parameter_sets_into_the_next_frame() {
        // 只拿到 SPS 时残留的字节必须能和新数据接起来，并且最终成为第一帧的开头。
        //
        // 这里刻意走字节层：参数集和它后面的 slice 属于同一个 AU，
        // 分两次调用聚合器会把参数集单独切出去（那正是真实踩到的 bug）。
        let mut bytes = nal(7, &[0x64, 0x00, 0x1f]);
        bytes.extend_from_slice(&nal(8, &[0xAA]));
        let (nals_a, carry_a) = parse_nals(&mut bytes);
        assert_eq!(nals_a.len(), 1, "SPS 有 PPS 的起始码来定界");
        assert_eq!(carry_a, nal(8, &[0xAA]));

        let (units_a, _) = group_access_units(nals_a, 0.0, 1.0);
        assert!(units_a.is_empty(), "只有参数集时不能出帧");

        // 下一轮：残留的 PPS + IDR slice + 下一个 slice。
        let mut bytes = carry_a;
        bytes.extend_from_slice(&nal(5, &[0xCC]));
        bytes.extend_from_slice(&p_slice_nal());
        let (nals_b, _) = parse_nals(&mut bytes);
        assert_eq!(nals_b.len(), 2, "PPS 和 IDR 都完整了");

        let (units_b, carry_b) = group_access_units(nals_b, 0.0, 1.0);
        // 最后一个 AU 一律不发：IDR 后面还没有"下一个起始码"来界定它的 slice，
        // 所以这一帧整个留到下一轮（与主循环拿下一批数据继续拼的行为一致）。
        assert!(units_b.is_empty(), "末尾 AU 的边界还没确定，不能发");
        // 但这一帧的数据必须完整地留在 carry 里，一个字节都不能丢。
        assert!(carry_b.starts_with(&nal(8, &[0xAA])), "PPS 必须在残留里");
        assert!(carry_b.ends_with(&nal(5, &[0xCC])), "IDR 必须在残留里");

        // 再补一个 slice 当边界，这一帧就成形了。
        let mut bytes = carry_b;
        bytes.extend_from_slice(&p_slice_nal());
        bytes.extend_from_slice(&p_slice_nal());
        let (nals_c, _) = parse_nals(&mut bytes);
        let (units_c, _) = group_access_units(nals_c, 0.0, 1.0);

        assert_eq!(units_c.len(), 1, "有了边界，参数集+IDR 组成一帧");
        assert!(units_c[0].key, "拼接后第一帧仍必须认出 IDR");
        assert!(units_c[0].data.starts_with(&nal(8, &[0xAA])), "PPS 在这一帧里");
        assert!(units_c[0].data.ends_with(&nal(5, &[0xCC])), "IDR 在这一帧里");
    }
}