<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import DeviceList from './components/DeviceList.vue'
import DiagnosticsView from './components/DiagnosticsView.vue'
import StatePill from './components/StatePill.vue'
import { api, asAppError, onAppEvent } from './lib/ipc'
import { ERROR_HINTS, type AppError, type AppState, type DeviceView, type LogEntry } from './lib/types'

const MAX_LOGS = 300
const SCAN_WINDOW_MS = 6_000

const state = ref<AppState>({ kind: 'idle' })
const devices = ref<DeviceView[]>([])
const logs = ref<LogEntry[]>([])
const version = ref('—')
const logPath = ref('')
const tab = ref<'live' | 'diagnostics'>('live')
const error = ref<AppError | null>(null)
const toast = ref('')

let unlisten: (() => void) | undefined
let toastTimer: number | undefined

const scanning = computed(() => state.value.kind === 'scanning')
const busy = computed(
  () => scanning.value || state.value.kind === 'connecting' || state.value.kind === 'stopping',
)
const connectedDevice = computed<DeviceView | null>(() => {
  const s = state.value
  return s.kind === 'ready' || s.kind === 'streaming' ? s.device : null
})
const canScan = computed(
  () => !busy.value && connectedDevice.value === null && state.value.kind !== 'no_adapter',
)
const errorHint = computed(() => (error.value ? ERROR_HINTS[error.value.code] : ''))

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

async function copyDiagnostics() {
  const text = await api.diagnosticsText()
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
  flash('诊断包已复制')
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
    if (snap.state.kind === 'error') {
      error.value = { code: snap.state.code, detail: snap.state.detail }
    }
  } catch (err) {
    error.value = asAppError(err)
  }

  unlisten = await onAppEvent((event) => {
    if (event.type === 'state-changed') {
      state.value = event.state
      if (event.state.kind === 'error') {
        error.value = { code: event.state.code, detail: event.state.detail }
      } else if (event.state.kind === 'ready') {
        error.value = null
      }
    } else if (event.type === 'devices') {
      devices.value = event.devices
    } else if (event.type === 'event-log') {
      pushLog(event.entry)
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
      <span class="ver">v{{ version }}</span>
      <button :disabled="busy" @click="detect">重新检测</button>
    </header>

    <nav class="tabs">
      <button :class="{ active: tab === 'live' }" @click="tab = 'live'">实时</button>
      <button :class="{ active: tab === 'diagnostics' }" @click="tab = 'diagnostics'">
        诊断
        <i v-if="logs.some((e) => e.level === 'error')" class="dot-err" />
      </button>
      <span class="spacer" />
      <span class="hint">按键映射 · 音质报告 → G4 / G5</span>
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
          <button v-if="scanning" @click="stopScan">停止扫描</button>
          <button v-else class="primary" :disabled="!canScan" @click="scan">扫描</button>
          <button v-if="connectedDevice" class="danger" @click="disconnect">断开</button>
        </div>
        <DeviceList
          :devices="devices"
          :connected-id="connectedDevice?.id ?? null"
          :busy="busy"
          :scanning="scanning"
          @connect="connect"
        />
      </section>

      <section class="card meter">
        <div class="card-head">
          <h2>电平与指标</h2>
          <span class="spacer" />
          <span class="tag">G2 接入</span>
        </div>
        <div class="placeholder">
          <p>这里将是实时电平表、缓冲深度、欠载与丢块计数。</p>
          <p class="dim">
            G0 只验证「应用身份 + 蓝牙权限 + 设备发现」，还没有音频路径，所以不显示任何数字——
            不编造读数。
          </p>
        </div>
      </section>
    </main>

    <main v-else class="diagwrap">
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
  padding: 0 16px 16px;
  gap: 12px;
}

.topbar {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 0 0;
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
  padding-bottom: 0;
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
  padding: 10px 12px;
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
  gap: 12px;
  min-height: 0;
  flex: 1;
}

.card {
  background: var(--panel);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius);
  padding: 12px;
  display: flex;
  flex-direction: column;
  min-height: 0;
}

.devices {
  flex: 1;
}

.card-head {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 8px;
}

.tag {
  font-size: 10.5px;
  letter-spacing: 0.04em;
  color: var(--dim);
  border: 1px solid var(--line);
  border-radius: 999px;
  padding: 1px 8px;
}

.meter {
  flex: 0 0 auto;
}

.placeholder {
  border: 1px dashed #2b3543;
  border-radius: 8px;
  padding: 12px 14px;
  background: #10151c;
}

.placeholder p {
  margin: 0;
  font-size: 12px;
}

.placeholder p + p {
  margin-top: 6px;
}

.dim {
  color: var(--dim);
  line-height: 1.7;
}

.diagwrap {
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
