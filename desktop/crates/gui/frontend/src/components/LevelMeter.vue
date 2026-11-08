<script setup lang="ts">
/**
 * 电平表：竖向条形 + 最近 10 秒 RMS 曲线，全部 canvas 手绘。
 *
 * 为什么不用 DOM 条/图表库：telemetry 是 5 Hz，但动画要 60 fps；
 * 每帧改 30 个 DOM 节点的宽高会明显掉帧（`docs/GUI.md` §5）。
 *
 * 只画，不算业务指标：rms/peak 都是后端给的，前端不做"我猜的丢块率"。
 *
 * **这里不画「缓冲」**：那是 sink 的 `buffer_ms`，在「延迟」指标组里已有同口径数字。
 * 同一屏出现两次同一个数（且与 `fill_p50` 长得像、口径不同）会让人没法判断
 * 哪个才是延迟——2026-11-07 版面重做时删掉了画布里的那一份。
 *
 * ---
 *
 * ## 性能（2026-11-07，G2 的 CPU 14.2% / 内存 208 MB 超标之后）
 *
 * `scripts/test-gui-perf.sh` 的分进程明细显示内存大头在 WebKit 的 WebContent（70 MB，
 * 渲染的就是这块 canvas）与 GPU helper（53 MB），主进程只 86 MB。
 *
 * ### 改了两件事，都经过实测比较
 *
 * **1. 静态内容不再每帧重画（保留）。** 原实现每帧都要：读 `clientWidth/clientHeight`
 * （每次读都强制同步布局）、重画 7 条网格线与 7 个刻度数字、每帧新建一次竖条渐变对象。
 * 实测待机 2 s 内 `fillText` 从 **1440 次降到 33 次**。
 *
 * **2. 但不再用「每帧贴一张大位图」的方式（这条试过、被否决）。**
 * 第一版把静态内容烘焙进一张离屏 canvas，每帧 `drawImage` 贴到主画布。
 * 用 CDP 的 `Performance.getMetrics` 量同一场景（推流 + 5 Hz 遥测，30 s ×2 轮）：
 *
 * | 版本 | TaskDuration | ScriptDuration | LayoutDuration |
 * | --- | --- | --- | --- |
 * | 原实现（每帧重画） | 845 / 785 ms | 249 / 238 ms | 38 / 36 ms |
 * | 离屏 + 每帧 blit | **955 / 1010 ms** | 217 / 252 ms | 44 / 40 ms |
 *
 * 脚本时间确实降了，但**总任务时间反而升了** —— 因为 1684×492 的位图按 60 fps 贴，
 * 等于每秒搬运约 200 MB。所以最终形态是**两层 canvas**：
 * 静态层是一块独立的 `<canvas>`，只在尺寸/数据变化时重画；动态层每帧只
 * `clearRect` 加画竖条和读数，**不做任何贴图**。
 *
 * ### 为什么不直接把帧率降到 30
 *
 * G2 的「前端 ≥ 30 fps」量的是真实渲染循环的帧率。把上限砍到 30 会让那条指标
 * 只剩零余量 —— 等于把判据改成"刚好达标"，而它原本要挡的是"电平表掉帧影响实时观感"。
 * 该降的是每帧的成本，不是帧率。推流态仍然跑 60 fps。
 *
 * 待机态确实降了帧（5 fps）：那时画面本来就是静止的（没有数据进来，也不编造读数），
 * 每秒重画 60 次同样的内容纯属浪费。窗口不可见时降到 2 fps（rAF 通常会被浏览器
 * 自己节流，但 Tauri 窗口被遮挡时的 WebKit 行为不保证，这里显式兜一层）。
 */
import { onMounted, onUnmounted, watch, ref } from 'vue'
import { api } from '../lib/ipc'

const props = defineProps<{
  rmsDbfs: number | null
  peakDbfs: number | null
  streaming: boolean
}>()

const FLOOR = -60
const CEIL = 0
const HISTORY = 50 // 5 Hz × 10 s

// --- 版面常量（画法只有一份，静态层与动态层共用）---
const PAD_TOP = 14
const HIST_H = 48
const METER_X = 46
const BAR_W = 30
const GRID_PAD = 14
const READOUT_DX = 26 // 读数区相对竖条右边缘的偏移
const TICKS = [-6, -12, -18, -24, -36, -48, -60]

/** 电平条下降时的释放速度（dB/s）。上升跟手，不设限。 */
const RELEASE_DB_PER_S = 24
/** 峰值保持线的衰减速度（dB/s）。 */
const PEAK_DECAY_DB_PER_S = 12

// --- 帧预算：按"用户看不看得见"分三档，单位 fps ---
const FPS_LIVE = 60 // 推流且可见：G2 帧率指标的测量条件
const FPS_IDLE = 5 // 待机：画面静止，重画是浪费
const FPS_HIDDEN = 2 // 窗口不可见：显式兜底，不依赖浏览器的 rAF 节流

const staticCanvas = ref<HTMLCanvasElement | null>(null)
const liveCanvas = ref<HTMLCanvasElement | null>(null)

const history: number[] = []

let staticCtx: CanvasRenderingContext2D | null = null
let liveCtx: CanvasRenderingContext2D | null = null

let raf = 0
/** 当前显示值（带释放） */
let level = FLOOR
let peakHold = FLOOR

// --- 尺寸缓存：绘制路径里**绝不**读 clientWidth / clientHeight ---
let cssW = 0
let cssH = 0
let dpr = 1

/** 竖条渐变。尺寸变化时失效。 */
let gradient: CanvasGradient | null = null

/** 上一次真正绘制的时刻（rAF 时间戳）。0 表示"下一帧立刻画"。 */
let lastDraw = 0
let hidden = false

function dbToRatio(db: number): number {
  const clamped = Math.max(FLOOR, Math.min(CEIL, db))
  return (clamped - FLOOR) / (CEIL - FLOOR)
}

/** 竖条可用高度。随容器高度变化，是所有版面计算的基准。 */
function meterHeight(): number {
  return Math.max(40, cssH - HIST_H - PAD_TOP - 16)
}

watch(
  () => props.rmsDbfs,
  () => {
    history.push(props.rmsDbfs ?? FLOOR)
    if (history.length > HISTORY) history.shift()
    // 曲线只有 5 Hz —— 只在这一刻重画静态层，而不是每帧重画 50 个点。
    drawStatic()
  },
)

// 推流起停是"整块画面换内容"（竖条出现/消失、读数亮暗、提示文字），
// 不能等帧预算到点再画。清掉 lastDraw 让下一帧立刻放行。
watch(
  () => props.streaming,
  () => {
    lastDraw = 0
  },
)

/**
 * 静态层：网格、刻度、固定标签、10 秒曲线。
 *
 * 只在**尺寸变化**、**dpr 变化**、**新数据到达**时重画。
 *
 * 绘制顺序与原实现严格一致：先网格/刻度，再竖条底槽（底槽会盖住穿过它的
 * 网格线段）。反过来会让网格线浮在底槽上，肉眼能看出差别。
 */
function drawStatic() {
  const c = staticCtx
  if (!c || cssW === 0 || cssH === 0) return

  const meterH = meterHeight()
  const gridRight = cssW - GRID_PAD
  const histTop = PAD_TOP + meterH + 22
  const histBottom = cssH - 6
  const tx = METER_X + BAR_W + READOUT_DX

  c.setTransform(dpr, 0, 0, dpr, 0, 0)
  c.clearRect(0, 0, cssW, cssH)

  // --- 刻度与网格 ---
  c.font = '10px ui-monospace, SFMono-Regular, Menlo, monospace'
  c.textAlign = 'right'
  c.textBaseline = 'middle'
  for (const db of TICKS) {
    const y = PAD_TOP + meterH * (1 - dbToRatio(db))
    c.fillStyle = '#4e586a'
    c.fillText(String(db), METER_X - 8, y)
    c.strokeStyle = '#1e2530'
    c.lineWidth = 1
    c.beginPath()
    c.moveTo(METER_X, y + 0.5)
    c.lineTo(gridRight, y + 0.5)
    c.stroke()
  }

  // --- 竖条底槽（在网格之后画，盖住穿过它的网格线段）---
  c.fillStyle = '#111721'
  c.fillRect(METER_X, PAD_TOP, BAR_W, meterH)

  // --- 固定标签 ---
  c.textAlign = 'left'
  c.textBaseline = 'alphabetic'
  c.font = '10px ui-monospace, Menlo, monospace'
  c.fillStyle = '#5b6577'
  c.fillText('dBFS · RMS', tx + 2, PAD_TOP + 42)

  // --- 曲线区底轴 ---
  c.strokeStyle = '#1e2530'
  c.lineWidth = 1
  c.beginPath()
  c.moveTo(METER_X, histBottom + 0.5)
  c.lineTo(gridRight, histBottom + 0.5)
  c.stroke()

  c.fillStyle = '#4e586a'
  c.font = '10px -apple-system, "PingFang SC", sans-serif'
  c.textAlign = 'right'
  c.fillText('最近 10 秒', gridRight, histTop + 8)

  // --- 最近 10 秒曲线 ---
  if (history.length > 1) {
    const step = (gridRight - METER_X - 4) / (HISTORY - 1)
    c.beginPath()
    history.forEach((v, i) => {
      const x = METER_X + i * step
      const y = histBottom - (histBottom - histTop) * dbToRatio(v)
      if (i === 0) c.moveTo(x, y)
      else c.lineTo(x, y)
    })
    c.strokeStyle = '#4c8dff'
    c.lineWidth = 1.6
    c.stroke()

    // 面积填充：让"这一段有没有声"一眼可见
    c.lineTo(METER_X + (history.length - 1) * step, histBottom)
    c.lineTo(METER_X, histBottom)
    c.closePath()
    c.fillStyle = '#4c8dff18'
    c.fill()
  }
}

/** 竖条渐变。缓存到尺寸变化为止（原来每帧新建一个）。 */
function barGradient(c: CanvasRenderingContext2D): CanvasGradient {
  if (gradient) return gradient
  const meterH = meterHeight()
  const g = c.createLinearGradient(0, PAD_TOP + meterH, 0, PAD_TOP)
  g.addColorStop(0, '#2fbf85')
  g.addColorStop(0.62, '#3ddc97')
  g.addColorStop(0.84, '#ffb020')
  g.addColorStop(1, '#ff5f5f')
  gradient = g
  return g
}

/** 动态层：只画会变的（竖条、峰值线、读数）。静态内容在它下面那张画布上。 */
function drawLive() {
  const c = liveCtx
  if (!c || cssW === 0 || cssH === 0) return
  const meterH = meterHeight()
  const tx = METER_X + BAR_W + READOUT_DX

  c.setTransform(dpr, 0, 0, dpr, 0, 0)
  c.clearRect(0, 0, cssW, cssH)

  // --- 竖条 + 峰值保持线 ---
  if (props.streaming) {
    const barTop = PAD_TOP + meterH * (1 - dbToRatio(level))
    c.fillStyle = barGradient(c)
    c.fillRect(METER_X, barTop, BAR_W, PAD_TOP + meterH - barTop)

    if (peakHold > FLOOR + 0.5) {
      const py = PAD_TOP + meterH * (1 - dbToRatio(peakHold))
      c.fillStyle = peakHold > -1 ? '#ff7b7b' : '#e8edf5'
      c.fillRect(METER_X - 4, Math.max(PAD_TOP, py - 1), BAR_W + 8, 2)
    }
  }

  // --- 读数 ---
  c.textAlign = 'left'
  c.textBaseline = 'alphabetic'

  c.fillStyle = props.streaming ? '#e8edf5' : '#37404f'
  c.font = '600 26px -apple-system, "SF Pro Text", sans-serif'
  c.fillText(props.rmsDbfs === null ? '—' : props.rmsDbfs.toFixed(1), tx, PAD_TOP + 26)

  c.font = '600 13px -apple-system, "PingFang SC", sans-serif'
  c.fillStyle = props.streaming ? '#c9d2e0' : '#37404f'
  c.fillText(
    props.peakDbfs === null ? '峰值 —' : `峰值 ${props.peakDbfs.toFixed(1)}`,
    tx,
    PAD_TOP + 68,
  )

  if (!props.streaming) {
    c.fillStyle = '#4e586a'
    c.font = '12px -apple-system, "PingFang SC", sans-serif'
    c.fillText('未推流：没有数据可画，也不编造读数', tx, PAD_TOP + 96)
  }
}

/** 当前应该按多少 fps 画。 */
function frameBudget(): number {
  if (hidden) return FPS_HIDDEN
  return props.streaming ? FPS_LIVE : FPS_IDLE
}

function loop(ts: number) {
  raf = requestAnimationFrame(loop)
  countFrame(ts)

  // dpr 变化（拖到外接屏）不会触发 ResizeObserver。读 devicePixelRatio
  // 只是取一个 JS 数值，不触发布局，所以可以每帧比一次。
  const ratio = window.devicePixelRatio || 1
  if (ratio !== dpr && cssW > 0 && cssH > 0) {
    resize(cssW, cssH, ratio)
    return
  }

  // 帧预算门控。0.9 的容差是为了让 vsync 的 ±1 ms 抖动不会被误判成"还没到点"。
  const minGap = 1000 / frameBudget()
  if (lastDraw && ts - lastDraw < minGap * 0.9) return

  const dt = lastDraw ? (ts - lastDraw) / 1000 : minGap / 1000
  lastDraw = ts

  const target = props.rmsDbfs ?? FLOOR
  // 上升跟手，下降按 24 dB/s 释放，读数不会一跳一跳。
  if (target >= level) level = target
  else level = Math.max(target, level - RELEASE_DB_PER_S * dt)

  const peakTarget = props.peakDbfs ?? FLOOR
  if (peakTarget >= peakHold) peakHold = peakTarget
  else peakHold = Math.max(FLOOR, peakHold - PEAK_DECAY_DB_PER_S * dt)

  drawLive()
}

/**
 * 帧率统计：每 `FPS_WINDOW_MS` 把这一段的实测帧率上报一次。
 *
 * 为什么挂在这里而不是另起一个 rAF 循环：**必须测真实渲染循环的帧率**。
 * 另开一个循环会与渲染争同一个 vsync，测出的数没有验收意义。
 *
 * 上报路径：`reportFps` → `apm-app` 诊断日志（`source=fps`）→
 * `scripts/test-gui-perf.sh` 从头到尾读出来算均值。
 * G2 的"前端 ≥ 30 fps"此前一直是"未测（没有采集手段）"，这就是那个手段。
 *
 * **只在推流且窗口可见时上报**：待机按 FPS_IDLE 画、不可见按 FPS_HIDDEN 画，
 * 那两个数字是我们的预算，不是渲染能力，混进 G2 的均值只会变成噪声。
 */
const FPS_WINDOW_MS = 5_000
let fpsWindowStart = 0
let fpsFrames = 0

function countFrame(ts: number) {
  if (!props.streaming || hidden) {
    // 不可测量区间：把窗口清掉，避免恢复可见时把两种节奏混进同一个均值。
    fpsWindowStart = 0
    fpsFrames = 0
    return
  }
  if (fpsWindowStart === 0) {
    fpsWindowStart = ts
    fpsFrames = 0
    return
  }
  fpsFrames += 1
  const elapsed = ts - fpsWindowStart
  if (elapsed >= FPS_WINDOW_MS) {
    const fps = (fpsFrames * 1000) / elapsed
    void reportFps(fps, elapsed)
    fpsWindowStart = ts
    fpsFrames = 0
  }
}

/** 上报一次帧率。失败静默 —— 性能采样不该影响用户。 */
async function reportFps(fps: number, windowMs: number) {
  try {
    await api.reportFps(fps, Math.round(windowMs))
  } catch {
    // 忽略：这是诊断通道，不影响功能
  }
}

/** 尺寸变化时重设两张位图并重画静态层。这是唯一会碰布局的地方。 */
function resize(w: number, h: number, ratio: number) {
  const st = staticCanvas.value
  const lv = liveCanvas.value
  if (!st || !lv || w <= 0 || h <= 0) return
  cssW = w
  cssH = h
  dpr = ratio
  const pxW = Math.max(1, Math.round(w * ratio))
  const pxH = Math.max(1, Math.round(h * ratio))
  // 只在真的变了才写 width/height：写它们会重新分配位图。
  for (const el of [st, lv]) {
    if (el.width !== pxW) el.width = pxW
    if (el.height !== pxH) el.height = pxH
  }
  staticCtx = st.getContext('2d')
  liveCtx = lv.getContext('2d')
  gradient = null
  drawStatic()
  lastDraw = 0
  drawLive()
}

const ro = new ResizeObserver((entries) => {
  const rect = entries[0]?.contentRect
  if (!rect) return
  resize(Math.round(rect.width), Math.round(rect.height), window.devicePixelRatio || 1)
})

function onVisibility() {
  hidden = document.visibilityState === 'hidden'
  // 从不可见切回可见时不该等满一个 2 fps 的间隔。
  if (!hidden) lastDraw = 0
}

onMounted(() => {
  hidden = document.visibilityState === 'hidden'
  document.addEventListener('visibilitychange', onVisibility)
  const st = staticCanvas.value
  if (st) {
    // 首次布局后先量一次：ResizeObserver 的回调是异步的，不先画会有空帧。
    const rect = st.getBoundingClientRect()
    resize(Math.round(rect.width), Math.round(rect.height), window.devicePixelRatio || 1)
    ro.observe(st)
  }
  raf = requestAnimationFrame(loop)
})

onUnmounted(() => {
  cancelAnimationFrame(raf)
  ro.disconnect()
  document.removeEventListener('visibilitychange', onVisibility)
})
</script>

<template>
  <!-- 两层画布：静态层在下（只画一次），动态层在上（每帧只画竖条与读数）。
       为什么不用「离屏 canvas + 每帧 drawImage」：1684×492 的位图按 60 fps 贴，
       等于每秒搬运约 200 MB，实测总任务时间反而升高（见文件头注释）。 -->
  <div class="meter">
    <canvas ref="staticCanvas" class="layer" />
    <canvas ref="liveCanvas" class="layer" />
  </div>
</template>

<style scoped>
/* 尺寸与背景由父级的 .meter-wrap 决定（边框、圆角、底色都在那边），
   这里只负责铺满，避免两层卡片套出双边框。 */
.meter {
  position: absolute;
  inset: 0;
}

.layer {
  position: absolute;
  inset: 0;
  display: block;
  width: 100%;
  height: 100%;
}
</style>
