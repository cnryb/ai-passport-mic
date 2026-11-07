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

const canvas = ref<HTMLCanvasElement | null>(null)
const history: number[] = []
let raf = 0
let level = FLOOR // 当前显示值（带释放）
let peakHold = FLOOR
let lastFrame = 0

function dbToRatio(db: number): number {
  const clamped = Math.max(FLOOR, Math.min(CEIL, db))
  return (clamped - FLOOR) / (CEIL - FLOOR)
}

watch(
  () => props.rmsDbfs,
  () => {
    history.push(props.rmsDbfs ?? FLOOR)
    if (history.length > HISTORY) history.shift()
  },
)

function draw() {
  const el = canvas.value
  if (!el) return
  const dpr = window.devicePixelRatio || 1
  const w = el.clientWidth
  const h = el.clientHeight
  if (w === 0 || h === 0) return
  if (el.width !== Math.round(w * dpr) || el.height !== Math.round(h * dpr)) {
    el.width = Math.round(w * dpr)
    el.height = Math.round(h * dpr)
  }
  const ctx = el.getContext('2d')
  if (!ctx) return
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
  ctx.clearRect(0, 0, w, h)

  // 版面：竖条在左，读数在它右边，曲线横贯底部。
  const padTop = 14
  const histH = 48
  const meterH = Math.max(40, h - histH - padTop - 16)
  const meterX = 46
  const barW = 30
  // 曲线画到整宽，不跟读数区抢空间。
  const gridRight = w - 14

  // --- 刻度与网格 ---
  ctx.font = '10px ui-monospace, SFMono-Regular, Menlo, monospace'
  ctx.textAlign = 'right'
  ctx.textBaseline = 'middle'
  for (const db of [-6, -12, -18, -24, -36, -48, -60]) {
    const y = padTop + meterH * (1 - dbToRatio(db))
    ctx.fillStyle = '#4e586a'
    ctx.fillText(String(db), meterX - 8, y)
    ctx.strokeStyle = '#1e2530'
    ctx.lineWidth = 1
    ctx.beginPath()
    ctx.moveTo(meterX, y + 0.5)
    ctx.lineTo(gridRight, y + 0.5)
    ctx.stroke()
  }

  // --- 竖条 ---
  ctx.fillStyle = '#111721'
  ctx.fillRect(meterX, padTop, barW, meterH)

  if (props.streaming) {
    const r = dbToRatio(level)
    const barTop = padTop + meterH * (1 - r)
    const grad = ctx.createLinearGradient(0, padTop + meterH, 0, padTop)
    grad.addColorStop(0, '#2fbf85')
    grad.addColorStop(0.62, '#3ddc97')
    grad.addColorStop(0.84, '#ffb020')
    grad.addColorStop(1, '#ff5f5f')
    ctx.fillStyle = grad
    ctx.fillRect(meterX, barTop, barW, padTop + meterH - barTop)

    // 峰值保持线
    if (peakHold > FLOOR + 0.5) {
      const py = padTop + meterH * (1 - dbToRatio(peakHold))
      ctx.fillStyle = peakHold > -1 ? '#ff7b7b' : '#e8edf5'
      ctx.fillRect(meterX - 4, Math.max(padTop, py - 1), barW + 8, 2)
    }
  }

  // --- 读数 ---
  const tx = meterX + barW + 26
  ctx.textAlign = 'left'
  ctx.textBaseline = 'alphabetic'

  ctx.fillStyle = props.streaming ? '#e8edf5' : '#37404f'
  ctx.font = '600 26px -apple-system, "SF Pro Text", sans-serif'
  ctx.fillText(
    props.rmsDbfs === null ? '—' : props.rmsDbfs.toFixed(1),
    tx,
    padTop + 26,
  )

  ctx.font = '10px ui-monospace, Menlo, monospace'
  ctx.fillStyle = '#5b6577'
  ctx.fillText('dBFS · RMS', tx + 2, padTop + 42)

  ctx.font = '600 13px -apple-system, "PingFang SC", sans-serif'
  ctx.fillStyle = props.streaming ? '#c9d2e0' : '#37404f'
  ctx.fillText(
    props.peakDbfs === null ? '峰值 —' : `峰值 ${props.peakDbfs.toFixed(1)}`,
    tx,
    padTop + 68,
  )

  if (!props.streaming) {
    ctx.fillStyle = '#4e586a'
    ctx.font = '12px -apple-system, "PingFang SC", sans-serif'
    ctx.fillText('未推流：没有数据可画，也不编造读数', tx, padTop + 96)
  }

  // --- 最近 10 秒曲线 ---
  const histTop = padTop + meterH + 22
  const histBottom = h - 6
  ctx.strokeStyle = '#1e2530'
  ctx.beginPath()
  ctx.moveTo(meterX, histBottom + 0.5)
  ctx.lineTo(gridRight, histBottom + 0.5)
  ctx.stroke()

  if (history.length > 1) {
    const step = (gridRight - meterX - 4) / (HISTORY - 1)
    ctx.beginPath()
    history.forEach((v, i) => {
      const x = meterX + i * step
      const y = histBottom - (histBottom - histTop) * dbToRatio(v)
      if (i === 0) ctx.moveTo(x, y)
      else ctx.lineTo(x, y)
    })
    ctx.strokeStyle = '#4c8dff'
    ctx.lineWidth = 1.6
    ctx.stroke()

    // 面积填充：让"这一段有没有声"一眼可见
    ctx.lineTo(meterX + (history.length - 1) * step, histBottom)
    ctx.lineTo(meterX, histBottom)
    ctx.closePath()
    ctx.fillStyle = '#4c8dff18'
    ctx.fill()
  }

  ctx.fillStyle = '#4e586a'
  ctx.font = '10px -apple-system, "PingFang SC", sans-serif'
  ctx.textAlign = 'right'
  ctx.fillText('最近 10 秒', gridRight, histTop + 8)
}

function loop(ts: number) {
  const dt = lastFrame ? (ts - lastFrame) / 1000 : 0.016
  lastFrame = ts
  countFrame(ts)

  const target = props.rmsDbfs ?? FLOOR
  // 上升跟手，下降按 24 dB/s 释放，读数不会一跳一跳。
  if (target >= level) level = target
  else level = Math.max(target, level - 24 * dt)

  const peakTarget = props.peakDbfs ?? FLOOR
  if (peakTarget >= peakHold) peakHold = peakTarget
  else peakHold = Math.max(FLOOR, peakHold - 12 * dt)

  draw()
  raf = requestAnimationFrame(loop)
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
 */
const FPS_WINDOW_MS = 5_000
let fpsWindowStart = 0
let fpsFrames = 0

function countFrame(ts: number) {
  if (fpsWindowStart === 0) {
    fpsWindowStart = ts
    fpsFrames = 0
    return
  }
  fpsFrames += 1
  const elapsed = ts - fpsWindowStart
  if (elapsed >= FPS_WINDOW_MS) {
    const fps = (fpsFrames * 1000) / elapsed
    // 只在**推流中**上报：待机时电平表基本不重绘，那个数字没有验收意义。
    if (props.streaming) {
      void reportFps(fps, elapsed)
    }
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

const ro = new ResizeObserver(draw)

onMounted(() => {
  raf = requestAnimationFrame(loop)
  // 容器尺寸变化（窗口缩放、设备卡展开把电平表压小）时立刻重绘一次，
  // 而不是等下一帧 rAF。rAF 在窗口不可见时会被节流，那时尺寸可能已经变了。
  ro.observe(canvas.value as HTMLCanvasElement)
})

onUnmounted(() => {
  cancelAnimationFrame(raf)
  ro.disconnect()
})
</script>

<template>
  <canvas ref="canvas" class="meter" />
</template>

<style scoped>
/* 尺寸与背景由父级的 .meter-wrap 决定（边框、圆角、底色都在那边），
   这里只负责铺满，避免两层卡片套出双边框。 */
.meter {
  display: block;
  width: 100%;
  height: 100%;
}
</style>
