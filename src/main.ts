import { invoke, Channel } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";

let greetInputEl: HTMLInputElement | null;
let greetMsgEl: HTMLElement | null;

function $(id: string) {
  return document.getElementById(id);
}

async function greet() {
  if (greetMsgEl && greetInputEl) {
    // Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
    greetMsgEl.textContent = await invoke("greet", {
      name: greetInputEl.value,
    });
  }
}

const canvas = $('canvas-video') as HTMLCanvasElement;
const ctx = canvas.getContext('2d');
let videoDecoder: VideoDecoder | null;
let frameCount = 0

function initDecoder() {
  videoDecoder = new VideoDecoder({
    output: (frame) => {
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight
      }

      ctx?.drawImage(frame, 0, 0);
      frame.close();
    },
    error: (e) => console.log('VideoDecoder error:', e),
  });
  if (videoDecoder === null) {
    console.log('failed to create VideoDecoder');
    return;
  }
  videoDecoder.configure({codec: 'avc1.42E01E', optimizeForLatency: true});
}

async function selectVideo() {
  try {
    const selected = await open({
      multiple: false,
      filters: [
        {
          name: "Video Files",
          extensions: ["mp4", "avi", "mkv", "mov", "wmv", "flv", "webm"],
        },
      ],
    });

    if (selected) {
      // selected 是文件路径字符串
      console.log("Selected file path:", selected);
      
      // 1. 显示文件路径到页面
      const videoPathEl = document.querySelector("#video-path");
      if (videoPathEl) {
        videoPathEl.textContent = `Selected: ${selected}`;
      }

      const onChunk = new Channel<ArrayBuffer>();
      onChunk.onmessage = (buffer: ArrayBuffer) => {
        if (!videoDecoder) {
          return;
        }

        frameCount++;
        const chunk = new EncodedVideoChunk({
          type: frameCount == 1 ? 'key' : 'delta',
          timestamp: frameCount * (1_000_000 / 30),
          data: new Uint8Array(buffer),
        });

        videoDecoder.decode(chunk)
      }

      
      // 2. 调用后端处理视频（示例）
      await invoke("start", { videoPath: selected, onChunk });
      
      // 3. 或者用于其他操作
      // - 预览视频
      // - 获取文件信息
      // - 转换格式等
    } else {
      console.log("No file selected");
    }
  } catch (error) {
    console.error("Error selecting file:", error);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  greetInputEl = document.querySelector("#greet-input");
  greetMsgEl = document.querySelector("#greet-msg");
  document.querySelector("#greet-form")?.addEventListener("submit", (e) => {
    e.preventDefault();
    greet();
  });

  $("select-video")?.addEventListener("click", selectVideo);
  initDecoder();
});