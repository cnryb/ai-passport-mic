<script setup lang="ts">
/**
 * 电平表：竖向条形 + 最近 10 秒 RMS 曲线，全部 canvas 手绘。
 *
 * 为什么不用 DOM 条/图表库：telemetry 是 5 Hz，但动画要 60 fps；
 * 每帧改 30 个 DOM 节点的宽高会明显掉帧（`docs/GUI.md` §5）。
 *
 * 只画，不算业务指标：rms/peak 都是后端给的，前端不做"我猜的丢块率"。
 */
import { onMounted, onUnmounted, ref, watch } from 'vue'
import { api } from '../lib/ipc'

const props = defineProps<{
  rmsDbfs: number | null
  peakDbfs: number | null
  streaming: boolean
  bufferMs: number
}>()

const FLOOR = -60
const CEIL = 0
const HISTORY = 50 // 5 Hz × 10 s
const BAR_W = 26

const canvas = ref<HTMLCanvasElement | null>(null)
const history = ref<number[]>([])
let raf = 0
let level = FLOOR // 当前显示值（带释放）
let peakHold = FLOOR
let lastFrame = 0

function dbToRatio(db: number): number {
  const clamped = Math.max(FLOOR, Math.min(CEIL, db))
  return (clamped - FLOOR) / (CEIL - FLOOR)
}

function pushSample() {
  const v = props.rmsDbfs ?? FLOOR
  history.value.push(v)
  if (history.value.length > HISTORY) history.value.shift()
}

watch(
  () => props.rmsDbfs,
  () => pushSample(),
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

  const histH = 42
  const meterH = h - histH - 8
  const meterX = 40
  const top = 6

  // --- 刻度 ---
  ctx.font = '10px ui-monospace, SFMono-Regular, Menlo, monospace'
  ctx.textAlign = 'right'
  ctx.textBaseline = 'middle'
  for (const db of [-6, -12, -18, -24, -36, -48, -60]) {
    const y = top + meterH * (1 - dbToRatio(db))
    ctx.fillStyle = '#5c6678'
    ctx.fillText(String(db), meterX - 6, y)
    ctx.strokeStyle = '#1c232d'
    ctx.lineWidth = 1
    ctx.beginPath()
    ctx.moveTo(meterX, y + 0.5)
    ctx.lineTo(w - 2, y + 0.5)
    ctx.stroke()
  }

  // --- 竖向条形 ---
  const r = dbToRatio(level)
  const barTop = top + meterH * (1 - r)
  const grad = ctx.createLinearGradient(0, top + meterH, 0, top)
  grad.addColorStop(0, '#3ddc97')
  grad.addColorStop(0.7, '#3ddc97')
  grad.addColorStop(0.86, '#ffb020')
  grad.addColorStop(1, '#ff5f5f')

  ctx.fillStyle = '#10151c'
  ctx.fillRect(meterX, top, BAR_W, meterH)
  ctx.fillStyle = grad
  ctx.fillRect(meterX, barTop, BAR_W, top + meterH - barTop)

  // 峰值保持线
  if (peakHold > FLOOR + 0.5) {
    const py = top + meterH * (1 - dbToRatio(peakHold))
    ctx.fillStyle = peakHold > -1 ? '#ff5f5f' : '#e7ecf4'
    ctx.fillRect(meterX - 3, Math.max(top, py - 1), BAR_W + 6, 2)
  }

  // --- 参数区（右侧文字）---
  const textX = meterX + BAR_W + 16
  ctx.textAlign = 'left'
  ctx.textBaseline = 'alphabetic'
  ctx.fillStyle = '#e7ecf4'
  ctx.font = '600 20px -apple-system, "SF Pro Text", sans-serif'
  ctx.fillText(props.rmsDbfs === null ? '—' : props.rmsDbfs.toFixed(1), textX, top + 26)
  ctx.font = '10px ui-monospace, Menlo, monospace'
  ctx.fillStyle = '#8b95a7'
  ctx.fillText('dBFS RMS', textX + 2, top + 42)

  ctx.font = '600 13px -apple-system, "SF Pro Text", sans-serif'
  ctx.fillStyle = '#e7ecf4'
  ctx.fillText(
    props.peakDbfs === null ? '峰值 —' : `峰值 ${props.peakDbfs.toFixed(1)}`,
    textX + 92,
    top + 26,
  )
  ctx.font = '10px ui-monospace, Menlo, monospace'
  ctx.fillStyle = '#8b95a7'
  ctx.fillText(`缓冲 ${props.bufferMs.toFixed(0)} ms`, textX + 94, top + 42)

  if (!props.streaming) {
    ctx.fillStyle = '#5c6678'
    ctx.font = '12px -apple-system, "PingFang SC", sans-serif'
    ctx.fillText('未推流：没有数据可画，也不编造读数', textX, top + 70)
  }

  // --- 最近 10 秒曲线 ---
  const histTop = top + meterH + 12
  const histBottom = h - 2
  const histLeft = meterX
  ctx.strokeStyle = '#1c232d'
  ctx.beginPath()
  ctx.moveTo(histLeft, histBottom + 0.5)
  ctx.lineTo(w - 2, histBottom + 0.5)
  ctx.stroke()

  if (history.value.length > 1) {
    const step = (w - histLeft - 2) / (HISTORY - 1)
    ctx.beginPath()
    history.value.forEach((v, i) => {
      const x = histLeft + i * step
      const y = histBottom - (histBottom - histTop) * dbToRatio(v)
      if (i === 0) ctx.moveTo(x, y)
      else ctx.lineTo(x, y)
    })
    ctx.strokeStyle = '#4c8dff'
    ctx.lineWidth = 1.5
    ctx.stroke()

    // 面积填充：让"这一段有没有声"一眼可见
    ctx.lineTo(histLeft + (history.value.length - 1) * step, histBottom)
    ctx.lineTo(histLeft, histBottom)
    ctx.closePath()
    ctx.fillStyle = '#4c8dff1a'
    ctx.fill()
  }

  ctx.fillStyle = '#5c6678'
  ctx.font = '10px -apple-system, "PingFang SC", sans-serif'
  ctx.textAlign = 'right'
  ctx.fillText('最近 10 秒', w - 4, histTop + 8)
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

onMounted(() => {
  raf = requestAnimationFrame(loop)
})

onUnmounted(() => {
  cancelAnimationFrame(raf)
})
</script>

<template>
  <canvas ref="canvas" class="meter" />
</template>

<style scoped>
.meter {
  width: 100%;
  height: 156px;
  display: block;
  background: var(--panel-2);
  border: 1px solid var(--line-soft);
  border-radius: 8px;
}
</style>
