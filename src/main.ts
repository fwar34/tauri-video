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

/** 一帧的元数据，必须和帧数据一起从 Rust 送过来（WebCodecs 需要每帧的时间戳）。 */
interface FrameSample {
  /** 显示时间戳（微秒） */
  timestamp: number;
  /** 时长（微秒） */
  duration: number;
  /** 是否关键帧（含 IDR slice） */
  key: boolean;
  /** Annex-B 格式的访问单元字节 */
  data: number[];
}

/** 后端的解码器配置消息。 */
interface ConfigMessage {
  type: "config";
  codec: string;
  width: number;
  height: number;
  descriptionLen: number;
}

/** 后端的成批帧消息。 */
interface SamplesMessage {
  type: "samples";
  samples: FrameSample[];
}

/** 后端处理完整个文件后发来的结束消息。 */
interface EndMessage {
  type: "end";
}

type ChannelMessage = ConfigMessage | SamplesMessage | EndMessage;

/** 解码后的帧队列上限。超过就丢最老的 delta 帧，避免内存无休止增长。 */
const MAX_QUEUED_FRAMES = 48;

/** 队列条目。关键帧标记必须自己记：`VideoFrame` 的类型定义里没有 `type`。 */
interface QueuedFrame {
  frame: VideoFrame;
  key: boolean;
}

/**
 * 播放时钟：按每帧自己的时间戳把画面送到 canvas。
 *
 * 时基用 `AudioContext.currentTime` 而不是 `performance.now()`：前者是音频时钟，
 * 以后要把声音接上时天然同步（音频才是真正的节拍器）。AudioContext 不接输出设备、
 * 只当时钟用，不会产生任何声音。
 *
 * 时间戳是流自己的时间轴（第一帧是 0），所以时钟记录"开始播放时对应的流时间"，
 * 用它做基准而不是直接用绝对时间。这样第一帧总能立刻显示，不会因为建立时钟的
 * 几百毫秒延迟而白等。
 */
class PlaybackClock {
  private audio: AudioContext | null = null;
  private perfBase = 0;
  private startedAt = 0;
  private baseTimestamp = 0;
  private started = false;

  private queue: QueuedFrame[] = [];
  private ended = false;
  private endedDrawn = false;
  private tickHandle = 0;
  private lastDrawAt = 0;

  /** 启动时基。必须在用户手势里调用，否则 AudioContext 会停在 suspended。 */
  async prepare() {
    if (this.started) {
      return;
    }
    this.started = true;
    try {
      this.audio = new AudioContext();
      await this.audio.resume();
    } catch {
      // 拿不到音频时钟就退回 performance.now()，只是精度差一些。
      this.audio = null;
    }
    this.perfBase = performance.now();
    this.startedAt = this.audio ? this.audio.currentTime : this.perfBase;
  }

  /** 自时钟启动以来经过的微秒数。 */
  private elapsedMicros(): number {
    if (this.audio) {
      return (this.audio.currentTime - this.startedAt) * 1_000_000;
    }
    return (performance.now() - this.perfBase) * 1000;
  }

  /**
   * @param key 该帧是否关键帧。调用方（解码前）就知道，不能依赖 VideoFrame。
   */
  push(frame: VideoFrame, key: boolean) {
    if (!this.started) {
      // prepare() 是异步的，理论上不会走到这里；真走到就退回"解码即显示"。
      this.draw(frame, 0);
      return;
    }

    if (this.ended && this.queue.length === 0) {
      // 已经收到结束消息还在往队列里塞帧，说明这批帧来得太晚，直接丢弃。
      frame.close();
      return;
    }

    if (this.lastDrawAt === 0 && this.queue.length === 0) {
      // 第一帧确定时间轴基准：把"流时间 0"对到"现在"。
      this.baseTimestamp = frame.timestamp;
      this.perfBase = performance.now();
      this.startedAt = this.audio ? this.audio.currentTime : this.perfBase;
    }

    // 队列过长说明解码远快于播放（正常现象）。丢掉最老的、还没有到显示时间的
    // 非关键帧；关键帧一律保留，丢掉它后面的帧就解不出来了。
    while (this.queue.length >= MAX_QUEUED_FRAMES) {
      const victim = this.queue.findIndex(
        (entry) => entry.frame.timestamp > this.lastDrawAt && !entry.key,
      );
      if (victim < 0) {
        break;
      }
      const [dropped] = this.queue.splice(victim, 1);
      dropped.frame.close();
    }

    this.queue.push({ frame, key });
    this.startTicking();
  }

  /** 数据流结束：把队列里剩下的帧按各自时间戳播完。 */
  markEnded() {
    this.ended = true;
    this.startTicking();
  }

  /** 换片 / 关闭时清空，释放还没显示的 VideoFrame。 */
  reset() {
    for (const entry of this.queue) {
      entry.frame.close();
    }
    this.queue = [];
    this.ended = false;
    this.endedDrawn = false;
    this.lastDrawAt = 0;
    this.started = false;
    this.baseTimestamp = 0;
    if (this.tickHandle) {
      cancelAnimationFrame(this.tickHandle);
      this.tickHandle = 0;
    }
    if (this.audio) {
      void this.audio.close();
      this.audio = null;
    }
  }

  private startTicking() {
    if (this.tickHandle) {
      return;
    }
    this.tickHandle = requestAnimationFrame(() => this.tick());
  }

  private tick() {
    this.tickHandle = 0;
    const target = this.baseTimestamp + this.elapsedMicros();

    // 整个 tick 必须包住：这里一旦抛异常，requestAnimationFrame 链就断了，
    // 画面会永久停住而且没有任何提示（`frame.transform` 不存在那次就是这样，
    // 排查了很久）。宁可打日志继续跑，也不要静默死掉。
    try {
      // 只画"时间已到"的帧，本 tick 最多画一张：落后很多时前面那些直接跳过，
      // 逐帧去补只会越追越慢。
      while (this.queue.length > 0) {
        const entry = this.queue[0];
        // 还没到显示时间：等下一次 rAF 再看。这里不把帧取出来，
        // 取出来又不画会漏掉 close()，直接把 VideoFrame 泄漏掉。
        if (entry.frame.timestamp > target && this.lastDrawAt > 0) {
          break;
        }
        this.queue.shift();
        this.draw(entry.frame, target);
        break;
      }
    } catch (e) {
      console.error('PlaybackClock.tick 失败:', e);
    }

    // 还有帧要放，或者流已结束但尾巴还没播完，就继续走时钟。
    const pending = this.queue.length > 0;
    if (pending || (this.ended && !this.endedDrawn)) {
      if (!pending) {
        this.endedDrawn = true;
      }
      this.startTicking();
    }
  }

  /** 诊断用：当前队列长度。 */
  get queued(): number {
    return this.queue.length;
  }

  private draw(frame: VideoFrame, _atMicros: number) {
    this.lastDrawAt = frame.timestamp;
    // 流的分辨率未必等于 canvas 的，统一按帧的显示尺寸调整 backing store。
    if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
      canvas.width = frame.displayWidth;
      canvas.height = frame.displayHeight;
    }
    // 不要调用 frame.transform()：WebView2/Chromium 的 VideoFrame 实现并不都有
    // 这个方法（实测报 "frame.transform is not a function"，一抛异常整个 tick 就
    // 断了，画面永远不刷新）。拿不到变换信息时按无变换绘制即可，绝大多数流本来
    // 就是恒等变换。
    ctx?.drawImage(frame, 0, 0);
    frame.close();
  }
}

const playback = new PlaybackClock();

/**
 * 把 Tauri 送来的原始载荷转成字节。
 *
 * 后端发的是 UTF-8 的 JSON 文本，走 Response 的 raw body 通道到达，
 * 可能是 ArrayBuffer，也可能是 Uint8Array，这里统一处理。
 */
function toBytes(payload: ArrayBuffer | Uint8Array | string): Uint8Array {
  if (typeof payload === "string") {
    return new TextEncoder().encode(payload);
  }
  if (payload instanceof Uint8Array) {
    return payload;
  }
  return new Uint8Array(payload);
}

/**
 * 剥掉可能存在的"JSON 字符串套 JSON"外层。
 *
 * 后端用 `Response::new(String)` 发送时，字符串可能被当作 JSON 值再编码一次，
 * 到达时就是 `"{\"type\":...}"` 这样的字面量；直接 JSON.parse 会得到一个
 * 字符串而不是对象，`message.type` 就成了 undefined。这里统一兼容两种形态。
 */
function unwrapJsonString(value: unknown): unknown {
  let current = value;
  // 最多剥两层，避免畸形数据把这里变成死循环。
  for (let i = 0; i < 2 && typeof current === "string"; i++) {
    const trimmed = current.trim();
    if (!trimmed.startsWith("{") && !trimmed.startsWith("[")) {
      break;
    }
    current = JSON.parse(trimmed);
  }
  return current;
}

/**
 * 处理后端推来的一条消息。
 *
 * 抛出异常会中断 Channel 的分派器、导致同一批里后面的消息全部丢失，
 * 所以调用方必须把整个过程包在 try/catch 里。
 */
function handleMessage(payload: ArrayBuffer | Uint8Array | string) {
  const text = new TextDecoder().decode(toBytes(payload));
  const message = unwrapJsonString(JSON.parse(text)) as ChannelMessage;

  switch (message.type) {
    case "config":
      applyConfig(message);
      break;
    case "samples":
      decodeSamples(message.samples);
      break;
    case "end":
      playback.markEnded();
      console.log(`流结束：共提交 ${decodedSampleCount} 个访问单元`);
      break;
    default:
      // 打印原始载荷的前 120 个字符：格式不对时这是唯一能看出问题的线索。
      console.warn("未知消息类型:", message, "原始载荷:", text.slice(0, 120));
  }
}

let videoDecoder: VideoDecoder | null = null;

/**
 * 已提交但还没吐出来的关键帧时间戳。
 *
 * `VideoDecoder` 的 output 回调不会告诉我们是哪个 chunk 产出这一帧，而
 * `VideoFrame` 上又读不到关键帧信息，所以只能按时间戳自己登记：提交时把关键帧
 * 的时间戳放进来，输出时对得上就说明这帧是关键帧。
 * 用 Set 而不是一个布尔量，是为了在解码有延迟/重排时也不会串帧。
 */
const pendingKeyTimestamps = new Set<number>();

/** 按后端给出的 codec / 分辨率初始化解码器。 */
function applyConfig(config: ConfigMessage) {
  if (config.width > 0 && config.height > 0) {
    canvas.width = config.width;
    canvas.height = config.height;
  }

  pendingKeyTimestamps.clear();

  videoDecoder = new VideoDecoder({
    // 这里不再直接画到 canvas：帧先入队，由播放时钟按时间戳送出。
    // 关键帧标记按时间戳查表得到——output 回调不会告诉我们是哪个 chunk
    // 产出这一帧，而 VideoFrame 上又读不到关键帧信息。
    output: (frame) => {
      const key = pendingKeyTimestamps.delete(frame.timestamp);
      playback.push(frame, key);
    },
    error: (e) => console.error('VideoDecoder error:', e),
  });

  // 流是 Annex-B，SPS/PPS 在每个关键帧的访问单元里带内传输，所以不需要
  // avcC description；descriptionLen 只用于日志核对。
  videoDecoder.configure({
    codec: config.codec,
    // 关键帧也要产出画面，交给播放时钟决定什么时候显示。
    optimizeForLatency: true,
  });

  console.log(
    `decoder configured: codec=${config.codec}, ${config.width}x${config.height}, 参数集 ${config.descriptionLen} 字节`,
  );
}

/** 数组里每个元素就是一个访问单元，正好对应一个 EncodedVideoChunk。 */
function decodeSamples(samples: FrameSample[]) {
  if (!videoDecoder || videoDecoder.state !== "configured") {
    return;
  }

  for (const sample of samples) {
    // 诊断：打印前若干个样本的 key 标记与类型序列。解码器报
    // "A key frame is required" 时，这里能直接看出是哪一帧的 key 判断错了。
    if (decodedSampleCount < 8) {
      console.log(
        `sample#${decodedSampleCount} ts=${sample.timestamp} key=${sample.key} bytes=${sample.data.length} head=${sample.data
          .slice(0, 8)
          .join(",")}`,
      );
    }
    decodedSampleCount++;

    try {
      if (sample.key) {
        pendingKeyTimestamps.add(sample.timestamp);
      }
      videoDecoder.decode(
        new EncodedVideoChunk({
          type: sample.key ? "key" : "delta",
          timestamp: sample.timestamp,
          duration: sample.duration,
          data: new Uint8Array(sample.data),
        }),
      );
    } catch (e) {
      pendingKeyTimestamps.delete(sample.timestamp);
      // 单帧失败不应该带崩整条流，打印后继续处理后面的帧。
      console.error("decode 失败:", e, sample.timestamp);
    }
  }
}

/** 已提交的解码样本数，仅用于限制诊断日志的输出量。 */
let decodedSampleCount = 0;

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

    if (!selected) {
      console.log("No file selected");
      return;
    }

    // selected 是文件路径字符串
    console.log("Selected file path:", selected);

    // 1. 显示文件路径到页面
    const videoPathEl = document.querySelector("#video-path");
    if (videoPathEl) {
      videoPathEl.textContent = `Selected: ${selected}`;
    }

    // 换片前把上一段彻底停掉并清空待播帧，否则上一段的画面会继续送出来。
    playback.reset();
    pendingKeyTimestamps.clear();
    decodedSampleCount = 0;
    if (videoDecoder && videoDecoder.state !== "closed") {
      videoDecoder.close();
    }
    videoDecoder = null;

    // 建立播放时钟。必须在这个用户手势里做，否则 AudioContext 会是 suspended。
    await playback.prepare();

    const onChunk = new Channel<ArrayBuffer>();
    onChunk.onmessage = (payload: ArrayBuffer) => {
      // 回调里抛出的异常会中断 Channel 的分派器，同批后续消息全部丢失，
      // 所以整体包一层 try/catch。
      try {
        handleMessage(payload);
      } catch (e) {
        console.error("onmessage 处理失败:", e);
      }
    };

    // 2. 调用后端处理视频
    await invoke("start", { videoPath: selected, onChunk });
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
});
