use std::{io, path::Path};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use ffmpeg_next::{format, codec};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
enum VideoError {
    #[error("文件未找到: {0}")]
    FileNotFound(String),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    FFmpegError(#[from] ffmpeg_next::Error),
}

impl serde::Serialize for VideoError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: serder::Serializer
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}

pub async fn start(video_path: &str) -> Result<(), VideoError>
{
    let video_path = Path::new(video_path);
    if !std::path::Path::exists(video_path) {
        return Error(VideoError::FileNotFound("找不到文件".to_string()));
    }

    if let Err(e) = ffmpeg_next::init() {
        
    }

    Ok(())
}