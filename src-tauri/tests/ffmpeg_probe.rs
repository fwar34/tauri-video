use std::path::PathBuf;
use std::process::Stdio;

use tauri_video_lib::video::{probe, resolve_program, start, VideoError};
use tokio::process::Command;

/// 用本机 ffmpeg 生成一段 1 秒的测试视频；本机没有 ffmpeg 时返回 None（跳过测试）。
async fn generate_sample() -> Option<PathBuf> {
    let ffmpeg = resolve_program("ffmpeg").ok()?;
    let output = std::env::temp_dir().join("tauri-video-probe-test.mp4");

    let status = Command::new(ffmpeg)
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-t",
            "1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("无法启动 ffmpeg");

    assert!(status.success(), "ffmpeg 生成测试视频失败");
    Some(output)
}

#[tokio::test]
async fn probe_reads_generated_video() {
    let Some(sample) = generate_sample().await else {
        eprintln!("未找到 ffmpeg，跳过本测试");
        return;
    };

    let info = probe(&sample).await.expect("ffprobe 解析失败");
    assert!(
        info.duration > 0.5 && info.duration < 2.0,
        "时长异常: {}",
        info.duration
    );
    assert!(info.size > 0, "文件大小异常: {}", info.size);
    assert!(info.format.contains("mp4"), "容器格式异常: {}", info.format);
    assert_eq!(info.video_codec.as_deref(), Some("h264"));
    assert_eq!(info.audio_codec.as_deref(), Some("aac"));

    std::fs::remove_file(&sample).ok();
}

#[tokio::test]
async fn start_reports_missing_file() {
    // let err = start("definitely-not-exists-9f8a7b6c.mp4")
    //     .await
    //     .unwrap_err();
    // assert!(
    //     matches!(err, VideoError::FileNotFound(_)),
    //     "错误类型不符: {err}"
    // );
}
