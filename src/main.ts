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

/**
 * 判断一段 Annex-B 数据里是否含有 IDR 帧（nal_unit_type == 5）。
 *
 * H.264 NAL 头是一个字节：forbidden_zero_bit(1) + nal_ref_idc(2) + nal_unit_type(5)，
 * 所以 type 就是 `byte & 0x1F`。只认 IDR，因为 SPS/PPS/SEI 虽然也是关键帧的
 * 组成部分，但单独一个 SPS 不能作为 key frame 提交。
 */
function isKeyFrame(data: Uint8Array): boolean {
  for (let i = 0; i + 3 < data.length; i++) {
    const isFourByte = data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 0 && data[i + 3] === 1;
    const isThreeByte = data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1;
    if (isFourByte || isThreeByte) {
      const headerIndex = i + (isFourByte ? 4 : 3);
      if (headerIndex < data.length && (data[headerIndex] & 0x1f) === 5) {
        return true;
      }
    }
  }
  return false;
}

async function selectVideo() {
  try {
    // 每次重新选片都要重置：frameCount 之前是模块级且从不清零，
    // 第二次拦截会继承上次的计数，帧类型和时间戳就全错了。
    frameCount = 0;

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
        // 这个回调里抛出的任何异常都会中断 Channel 的分派器，
        // 同一批里后面的消息会全部丢失，所以整体包一层 try/catch。
        try {
          if (!videoDecoder) {
            return;
          }

          console.log(`buffer len:${buffer.byteLength}`);

          frameCount++;
          const bytes = new Uint8Array(buffer);
          const chunk = new EncodedVideoChunk({
            // 靠 NAL 类型判断关键帧，比用计数器可靠：
            // 计数器在换视频时不重置，会把新视频的首帧标成 delta。
            type: isKeyFrame(bytes) ? 'key' : 'delta',
            timestamp: frameCount * (1_000_000 / 30),
            data: bytes,
          });

          if (videoDecoder.state === 'configured') {
            videoDecoder.decode(chunk);
          }
        } catch (e) {
          console.error('onmessage 处理失败:', e);
        }
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