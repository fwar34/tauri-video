import { invoke } from "@tauri-apps/api/core";
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
      
      // 2. 调用后端处理视频（示例）
      await invoke("start", { videoPath: selected });
      
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

  document.getElementById("select-video")?.addEventListener("click", selectVideo);
});