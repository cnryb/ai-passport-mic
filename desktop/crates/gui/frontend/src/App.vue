<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import DeviceList from './components/DeviceList.vue'
import DiagnosticsView from './components/DiagnosticsView.vue'
import KeymapView from './components/KeymapView.vue'
import LevelMeter from './components/LevelMeter.vue'
import QualityView from './components/QualityView.vue'
import StatePill from './components/StatePill.vue'
import { api, asAppError, onAppEvent } from './lib/ipc'
import {
  ERROR_HINTS,
  type AppError,
  type AppState,
  type BindingInput,
  type DeviceView,
  type KeymapView as KeymapData,
  type LogEntry,
  type QualityReport,
  type SessionRecord,
  type SessionSummary,
  type SinkView,
  type Telemetry,
} from './lib/types'

const MAX_LOGS = 300
const SCAN_WINDOW_MS = 6_000

type Tab = 'live' | 'quality' | 'keymap' | 'diagnostics'

const state = ref<AppState>({ kind: 'idle' })
const devices = ref<DeviceView[]>([])
const logs = ref<LogEntry[]>([])
const version = ref('—')
const logPath = ref('')
const tab = ref<Tab>('live')
const error = ref<AppError | null>(null)
const toast = ref('')

const sinks = ref<SinkView[]>([])
const sink = ref<string | null>(null)
const targetMs = ref(80)
const saveWav = ref(false)
const autoConnect = ref(false)
const lastDevice = ref<string | null>(null)

const telemetry = ref<Telemetry | null>(null)
const summary = ref<SessionSummary | null>(null)
const lastSession = ref<SessionRecord | null>(null)
/** 设备电量百分比；null = 未连接或电量计不可用。设备只在变化时推送。 */
const battery = ref<number | null>(null)

const report = ref<QualityReport | null>(null)
const reportLoading = ref(false)
const reportError = ref<string | null>(null)

const keymap = ref<KeymapData | null>(null)
const keymapBusy = ref(false)
const keymapError = ref<string | null>(null)

let unlisten: (() => void) | undefined
let toastTimer: number | undefined

const stateKind = computed(() => state.value.kind)
const scanning = computed(() => stateKind.value === 'scanning')
const streaming = computed(() => stateKind.value === 'streaming')
const busy = computed(
  () => scanning.value || stateKind.value === 'connecting' || stateKind.value === 'stopping',
)
const connectedDevice = computed<DeviceView | null>(() => {
  const s = state.value
  return s.kind === 'ready' || s.kind === 'streaming' ? s.device : null
})
const canScan = computed(
  () => !busy.value && connectedDevice.value === null && stateKind.value !== 'no_adapter',
)
const canStart = computed(() => stateKind.value === 'ready' && sink.value !== null)
const errorHint = computed(() => (error.value ? ERROR_HINTS[error.value.code] : ''))

const wavPath = computed(() => summary.value?.wav_path ?? lastSession.value?.wav_path ?? null)
const activeSessionId = computed(() => summary.value?.session_id ?? lastSession.value?.session_id ?? null)

/** 与固件 battery.c 的低电量阈值一致（APM_BATTERY_LOW_SOC_PCT = 20）。 */
const BATTERY_LOW_PCT = 20
const batteryLow = computed(() => battery.value !== null && battery.value <= BATTERY_LOW_PCT)
const batteryText = computed(() => (battery.value === null ? null : `电量 ${battery.value}%`))

const elapsedText = computed(() => {
  const ms = streaming.value ? (telemetry.value?.elapsed_ms ?? 0) : 0
  const total = Math.floor(ms / 1000)
  return `${String(Math.floor(total / 60)).padStart(2, '0')}:${String(total % 60).padStart(2, '0')}`
})

/** 指标条：口径写死在 docs/GUI.md §7，前端不自己算丢块率。 */
const metrics = computed(() => {
  const t = telemetry.value
  if (!t) {
    return [
      { label: '块', value: '—' },
      { label: '丢块', value: '—' },
      { label: '设备丢弃', value: '—' },
      { label: '欠载', value: '—' },
      { label: '缓冲', value: '—' },
    ]
  }
  return [
    { label: '块', value: String(t.blocks) },
    { label: '丢块', value: String(t.lost_blocks) },
    { label: '跳变', value: String(t.seq_jumps) },
    { label: '设备丢弃', value: String(t.device_dropped) },
    { label: '欠载', value: String(t.underruns) },
    { label: '静音帧', value: String(t.silent_frames) },
    { label: '锁定失败', value: String(t.lock_misses) },
    { label: '溢出', value: String(t.overflow_drops) },
    { label: '缓冲', value: `${t.buffer_ms.toFixed(0)} ms` },
    { label: '漂移', value: `${t.ppm >= 0 ? '+' : ''}${t.ppm.toFixed(0)} ppm` },
  ]
})

function flash(message: string) {
  toast.value = message
  if (toastTimer) window.clearTimeout(toastTimer)
  toastTimer = window.setTimeout(() => (toast.value = ''), 2_200)
}

async function handle(fn: () => Promise<unknown>) {
  try {
    await fn()
  } catch (err) {
    error.value = asAppError(err)
  }
}

async function copyText(text: string, okMessage: string) {
  try {
    await navigator.clipboard.writeText(text)
  } catch {
    // WKWebView 在自定义协议下可能没有 navigator.clipboard，回退到 execCommand。
    const area = document.createElement('textarea')
    area.value = text
    area.style.position = 'fixed'
    area.style.opacity = '0'
    document.body.appendChild(area)
    area.select()
    document.execCommand('copy')
    area.remove()
  }
  flash(okMessage)
}

// ---- 设备 ----

async function detect() {
  error.value = null
  await handle(() => api.detectAdapter())
}

async function scan() {
  error.value = null
  await handle(() => api.scanStart(SCAN_WINDOW_MS))
}

async function stopScan() {
  await handle(() => api.scanStop())
}

async function connect(id: string) {
  error.value = null
  await handle(() => api.connect(id))
}

async function disconnect() {
  await handle(() => api.disconnect())
}

async function onAutoConnectChange() {
  await handle(() => api.setAutoConnect(autoConnect.value))
  flash(autoConnect.value ? '启动时将自动连接上次的设备' : '已关闭启动自动连接')
}

// ---- 声卡 / 推流 ----

async function refreshSinks() {
  await handle(async () => {
    sinks.value = await api.listSinks()
    if (!sink.value) {
      // 默认优先选 BlackHole：这是 macOS 上最常见的虚拟声卡，且名字能认出来。
      const preferred =
        sinks.value.find((s) => s.name.toLowerCase().includes('blackhole') && s.supports_16k) ??
        sinks.value.find((s) => s.supports_16k)
      if (preferred) {
        sink.value = preferred.name
        await api.setSink(preferred.name)
      }
    }
  })
}

async function onSinkChange() {
  if (!sink.value) return
  const picked = sinks.value.find((s) => s.name === sink.value)
  if (picked && !picked.supports_16k) {
    // 先拦住再报：`Sink::open` 会拒绝，那时用户已经在等"开始"了。
    error.value = {
      code: 'sink_rate_unsupported',
      detail: `${picked.name} 不支持 16000 Hz 原生采样率；本项目不做隐式重采样`,
    }
  }
  await handle(() => api.setSink(sink.value as string))
}

async function onBufferChange() {
  await handle(() => api.setBufferTarget(targetMs.value))
}

async function onSaveWavChange() {
  // 勾选状态要持久化：隐私口径见 docs/GUI.md §12 第 3 条，默认关。
  await handle(() => api.setSaveWav(saveWav.value))
  flash(saveWav.value ? '会话 WAV 将保存到应用数据目录' : '会话不落盘')
}

async function startStream() {
  error.value = null
  telemetry.value = null
  report.value = null
  reportError.value = null
  await handle(async () => {
    const id = await api.startStream(saveWav.value)
    lastSession.value = { session_id: id, wav_path: null, started_ms: Date.now() }
  })
}

async function stopStream() {
  await handle(async () => {
    const s = await api.stopStream()
    summary.value = s
    telemetry.value = null
    if (s.wav_path) {
      tab.value = 'quality'
      await analyze()
    }
  })
}

// ---- 音质报告 ----

async function analyze() {
  const id = activeSessionId.value
  if (!id) return
  reportLoading.value = true
  reportError.value = null
  try {
    report.value = await api.analyzeSession(id)
  } catch (err) {
    const e = asAppError(err)
    reportError.value = `${e.code}: ${e.detail}`
    report.value = null
  } finally {
    reportLoading.value = false
  }
}

async function copyReportJson() {
  const id = activeSessionId.value
  if (!id) return
  await handle(async () => {
    const text = await api.analyzeSessionJson(id)
    await copyText(text, '报告 JSON 已复制')
  })
}

async function revealWav() {
  const path = wavPath.value
  if (!path) return
  await handle(() => api.revealPath(path))
}

// ---- 按键映射 ----

async function loadKeymap() {
  try {
    keymap.value = await api.keymapView()
  } catch (err) {
    keymapError.value = asAppError(err).detail
  }
}

async function saveKeymap(bindings: BindingInput[]) {
  keymapBusy.value = true
  keymapError.value = null
  try {
    keymap.value = await api.keymapSave(bindings)
    flash('映射已保存')
  } catch (err) {
    keymapError.value = asAppError(err).detail
  } finally {
    keymapBusy.value = false
  }
}

async function importKeymap(text: string) {
  keymapBusy.value = true
  keymapError.value = null
  try {
    keymap.value = await api.keymapImport(text)
    flash('映射已导入')
  } catch (err) {
    keymapError.value = asAppError(err).detail
  } finally {
    keymapBusy.value = false
  }
}

async function resetKeymap() {
  keymapBusy.value = true
  keymapError.value = null
  try {
    keymap.value = await api.keymapReset()
    flash('已恢复默认映射')
  } catch (err) {
    keymapError.value = asAppError(err).detail
  } finally {
    keymapBusy.value = false
  }
}

async function copyKeymap() {
  await handle(async () => {
    const text = await api.keymapText()
    await copyText(text, '映射 JSON 已复制')
  })
}

function switchTab(next: Tab) {
  tab.value = next
  if (next === 'keymap') void loadKeymap()
}

// ---- 诊断 ----

async function copyDiagnostics() {
  const text = await api.diagnosticsText()
  await copyText(text, '诊断包已复制')
}

async function revealLog() {
  await handle(() => api.revealLog())
}

function pushLog(entry: LogEntry) {
  const next = logs.value.length >= MAX_LOGS ? logs.value.slice(1) : logs.value.slice()
  next.push(entry)
  logs.value = next
}

onMounted(async () => {
  try {
    const snap = await api.snapshot()
    version.value = snap.version
    state.value = snap.state
    devices.value = snap.devices
    logs.value = snap.log_tail
    logPath.value = snap.log_path
    lastSession.value = snap.last_session
    targetMs.value = snap.settings.target_ms
    saveWav.value = snap.settings.save_wav
    autoConnect.value = snap.settings.auto_connect
    lastDevice.value = snap.settings.last_device
    sink.value = snap.settings.sink
    if (snap.state.kind === 'error') {
      error.value = { code: snap.state.code, detail: snap.state.detail }
    }
  } catch (err) {
    error.value = asAppError(err)
  }

  await refreshSinks()
  await loadKeymap()

  unlisten = await onAppEvent((event) => {
    if (event.type === 'state-changed') {
      state.value = event.state
      if (event.state.kind === 'error') {
        error.value = { code: event.state.code, detail: event.state.detail }
      } else if (event.state.kind === 'ready') {
        error.value = null
      }
      // 掉出 ready/streaming 就没有链路了，电量清零避免显示旧值。
      const kind = event.state.kind
      if (kind !== 'ready' && kind !== 'streaming') {
        battery.value = null
      }
    } else if (event.type === 'devices') {
      devices.value = event.devices
    } else if (event.type === 'event-log') {
      pushLog(event.entry)
    } else if (event.type === 'telemetry') {
      telemetry.value = event.telemetry
    } else if (event.type === 'session-done') {
      summary.value = event.summary
    } else if (event.type === 'battery') {
      battery.value = event.pct
    }
  })
})

onUnmounted(() => {
  unlisten?.()
  if (toastTimer) window.clearTimeout(toastTimer)
})
</script>

<template>
  <div class="app">
    <header class="topbar">
      <StatePill :state="state" />
      <div class="ident">
        <strong>{{ connectedDevice?.name ?? 'AI Passport Mic' }}</strong>
        <span v-if="connectedDevice" class="mono sub">{{ connectedDevice.id.slice(0, 12) }}…</span>
      </div>
      <span class="spacer" />
      <span
        v-if="batteryText && connectedDevice"
        class="meta mono"
        :class="{ 'batt-low': batteryLow }"
      >
        {{ batteryText }}{{ batteryLow ? '，请充电' : '' }}
      </span>
      <span v-if="connectedDevice?.rssi != null" class="meta mono">{{ connectedDevice.rssi }} dBm</span>
      <span v-if="streaming" class="meta mono">缓冲 {{ telemetry?.buffer_ms.toFixed(0) ?? '—' }} ms</span>
      <span v-if="streaming" class="meta mono">{{ elapsedText }}</span>
      <span class="ver">v{{ version }}</span>
      <button :disabled="busy" @click="detect">重新检测</button>
    </header>

    <nav class="tabs">
      <button :class="{ active: tab === 'live' }" @click="switchTab('live')">实时</button>
      <button :class="{ active: tab === 'quality' }" @click="switchTab('quality')">音质报告</button>
      <button :class="{ active: tab === 'keymap' }" @click="switchTab('keymap')">按键映射</button>
      <button :class="{ active: tab === 'diagnostics' }" @click="switchTab('diagnostics')">
        诊断
        <i v-if="logs.some((e) => e.level === 'error')" class="dot-err" />
      </button>
      <span class="spacer" />
      <span v-if="stateKind === 'no_adapter'" class="hint">没有可用蓝牙适配器</span>
    </nav>

    <div v-if="error" class="banner">
      <div class="banner-main">
        <strong>{{ error.code }}</strong>
        <span>{{ error.detail }}</span>
      </div>
      <div class="banner-hint">{{ errorHint }}</div>
      <button class="ghost" @click="error = null">收起</button>
    </div>

    <main v-if="tab === 'live'" class="live">
      <section class="card devices">
        <div class="card-head">
          <h2>设备</h2>
          <span class="spacer" />
          <label class="inline" title="启动后自动连接上次的设备（设备需已上电并在广播）">
            <input v-model="autoConnect" type="checkbox" @change="onAutoConnectChange" />
            自动连接
          </label>
          <button v-if="scanning" @click="stopScan">停止扫描</button>
          <button v-else class="primary" :disabled="!canScan" @click="scan">扫描</button>
          <button v-if="connectedDevice" class="danger" :disabled="streaming" @click="disconnect">
            断开
          </button>
        </div>
        <DeviceList
          :devices="devices"
          :connected-id="connectedDevice?.id ?? null"
          :busy="busy"
          :scanning="scanning"
          @connect="connect"
        />
        <p v-if="stateKind === 'idle' && lastDevice" class="tip">
          上次连接：{{ lastDevice }}{{ autoConnect ? '，启动时自动连接' : '' }}
        </p>
      </section>

      <section class="card meter">
        <div class="card-head">
          <h2>电平与指标</h2>
          <span class="spacer" />
          <span class="tag" :data-live="streaming">{{ streaming ? 'LIVE' : '待机' }}</span>
        </div>
        <LevelMeter
          :rms-dbfs="telemetry?.rms_dbfs ?? null"
          :peak-dbfs="telemetry?.peak_dbfs ?? null"
          :streaming="streaming"
          :buffer-ms="telemetry?.buffer_ms ?? 0"
        />
        <div class="metrics">
          <span v-for="m in metrics" :key="m.label">
            <i>{{ m.label }}</i>
            <b>{{ m.value }}</b>
          </span>
        </div>
      </section>

      <section class="card controls">
        <div class="ctl-row">
          <label for="sink">声卡</label>
          <select id="sink" v-model="sink" :disabled="streaming" @change="onSinkChange">
            <option :value="null" disabled>选择输出设备…</option>
            <option v-for="s in sinks" :key="s.name" :value="s.name">
              {{ s.name }}{{ s.supports_16k ? '' : '（不支持 16 kHz）' }}
            </option>
          </select>
          <button class="ghost" :disabled="streaming" @click="refreshSinks">重新枚举</button>
          <span class="spacer" />
          <label class="inline">
            <input v-model="saveWav" type="checkbox" :disabled="streaming" @change="onSaveWavChange" />
            同时保存 WAV
          </label>
        </div>

        <div class="ctl-row">
          <label for="buf">缓冲</label>
          <input
            id="buf"
            v-model.number="targetMs"
            type="range"
            min="40"
            max="80"
            step="5"
            @change="onBufferChange"
          />
          <span class="mono val">{{ targetMs }} ms</span>
          <span class="spacer" />
          <button v-if="!streaming" class="primary" :disabled="!canStart" @click="startStream">
            开始
          </button>
          <button v-else class="danger" @click="stopStream">停止</button>
        </div>

        <p v-if="!streaming && stateKind === 'ready' && !sink" class="tip">
          还没选输出声卡。macOS 上通常装 BlackHole 2ch，然后在会议软件里把麦克风选成它。
        </p>
        <p v-else-if="!streaming && stateKind === 'ready'" class="tip">
          开始后，在会议软件（腾讯会议 / Zoom / 飞书）里把麦克风选成「{{ sink }}」。
        </p>
      </section>
    </main>

    <main v-else-if="tab === 'quality'" class="wrap">
      <QualityView
        :report="report"
        :session-id="activeSessionId"
        :wav-path="wavPath"
        :loading="reportLoading"
        :error="reportError"
        @analyze="analyze"
        @copy-json="copyReportJson"
        @reveal="revealWav"
      />
    </main>

    <main v-else-if="tab === 'keymap'" class="wrap">
      <KeymapView
        :view="keymap"
        :busy="keymapBusy"
        :error="keymapError"
        @save="saveKeymap"
        @reset="resetKeymap"
        @import="importKeymap"
        @copy="copyKeymap"
      />
    </main>

    <main v-else class="wrap">
      <DiagnosticsView
        :entries="logs"
        :log-path="logPath"
        @copy="copyDiagnostics"
        @reveal="revealLog"
      />
    </main>

    <transition name="fade">
      <div v-if="toast" class="toast">{{ toast }}</div>
    </transition>
  </div>
</template>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  height: 100%;
  padding: 0 16px 14px;
  gap: 10px;
}

.topbar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 12px 0 0;
}

.ident {
  display: flex;
  align-items: baseline;
  gap: 8px;
  min-width: 0;
}

.ident strong {
  font-size: 13.5px;
  font-weight: 600;
  white-space: nowrap;
}

.sub {
  color: var(--dim);
}

.meta {
  color: var(--muted);
  font-size: 11.5px;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}

.meta.batt-low {
  color: var(--warn);
  font-weight: 600;
}

.spacer {
  flex: 1;
}

.ver {
  color: var(--dim);
  font-size: 11.5px;
  font-variant-numeric: tabular-nums;
}

.tabs {
  display: flex;
  align-items: center;
  gap: 4px;
  border-bottom: 1px solid var(--line-soft);
}

.tabs button {
  border: none;
  background: transparent;
  border-radius: 0;
  padding: 7px 10px;
  color: var(--muted);
  font-size: 12.5px;
  border-bottom: 2px solid transparent;
  margin-bottom: -1px;
}

.tabs button:hover {
  color: var(--text);
  background: transparent;
}

.tabs button.active {
  color: var(--text);
  border-bottom-color: var(--accent);
}

.hint {
  font-size: 11.5px;
  color: var(--dim);
  padding-right: 2px;
}

.dot-err {
  display: inline-block;
  width: 5px;
  height: 5px;
  border-radius: 50%;
  background: var(--err);
  margin-left: 5px;
  vertical-align: middle;
}

.banner {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto;
  gap: 4px 12px;
  padding: 9px 12px;
  border-radius: var(--radius);
  border: 1px solid #ff5f5f3d;
  background: #ff5f5f10;
}

.banner-main {
  display: flex;
  gap: 10px;
  align-items: baseline;
  min-width: 0;
}

.banner-main strong {
  color: var(--err);
  font-size: 11.5px;
  font-weight: 600;
  white-space: nowrap;
}

.banner-main span {
  font-size: 12px;
  color: #f2d6d6;
  overflow-wrap: anywhere;
}

.banner-hint {
  grid-column: 1;
  font-size: 11.5px;
  color: var(--muted);
}

.banner button {
  grid-row: 1 / span 2;
  grid-column: 2;
  align-self: center;
}

.live {
  display: flex;
  flex-direction: column;
  gap: 10px;
  min-height: 0;
  flex: 1;
}

.card {
  background: var(--panel);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius);
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  min-height: 0;
}

.devices {
  flex: 1;
  min-height: 96px;
  max-height: 190px;
  overflow: hidden;
}

.meter {
  flex: 0 0 auto;
  gap: 8px;
}

.controls {
  flex: 0 0 auto;
  gap: 8px;
}

.card-head {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 6px;
}

.tag {
  font-size: 10px;
  letter-spacing: 0.06em;
  color: var(--dim);
  border: 1px solid var(--line);
  border-radius: 999px;
  padding: 1px 8px;
}

.tag[data-live='true'] {
  color: var(--ok);
  border-color: #3ddc9755;
  background: #3ddc970f;
}

.metrics {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 14px;
  padding-top: 2px;
}

.metrics span {
  display: inline-flex;
  align-items: baseline;
  gap: 5px;
  font-size: 11px;
}

.metrics i {
  color: var(--dim);
  font-style: normal;
}

.metrics b {
  font-weight: 600;
  font-variant-numeric: tabular-nums;
}

.ctl-row {
  display: flex;
  align-items: center;
  gap: 8px;
}

.ctl-row > label {
  color: var(--muted);
  font-size: 12px;
  min-width: 30px;
}

select {
  font: inherit;
  font-size: 12px;
  color: var(--text);
  background: var(--panel-2);
  border: 1px solid var(--line);
  border-radius: 7px;
  padding: 4px 8px;
  min-width: 210px;
}

select:focus {
  outline: none;
  border-color: #4c8dff88;
}

input[type='range'] {
  width: 180px;
  accent-color: var(--accent);
}

.inline {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 12px;
  color: var(--muted);
}

.inline input {
  accent-color: var(--accent);
}

.val {
  color: var(--muted);
  min-width: 48px;
}

.tip {
  margin: 0;
  font-size: 11px;
  color: var(--dim);
  line-height: 1.6;
}

.wrap {
  flex: 1;
  min-height: 0;
}

.toast {
  position: fixed;
  right: 18px;
  bottom: 16px;
  background: #1e2733;
  border: 1px solid #2f3d4e;
  color: var(--text);
  padding: 7px 14px;
  border-radius: 8px;
  font-size: 12px;
  box-shadow: 0 8px 24px #00000055;
}

.fade-enter-active,
.fade-leave-active {
  transition: opacity 0.18s;
}

.fade-enter-from,
.fade-leave-to {
  opacity: 0;
}
</style>
