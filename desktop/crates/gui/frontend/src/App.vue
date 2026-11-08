<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref } from 'vue'
import DeviceList from './components/DeviceList.vue'
import DiagnosticsView from './components/DiagnosticsView.vue'
import FirmwareView from './components/FirmwareView.vue'
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
  type SinkCheckReport,
  type SinkView,
  type Telemetry,
} from './lib/types'

const MAX_LOGS = 300
const SCAN_WINDOW_MS = 6_000

/**
 * 版面结构（改布局时先读这段）：
 *
 *   顶栏 42px  /  Tabs 36px  /  错误条 34px  /  内容区（唯一滚动）  /  底部操作条 56px
 *
 * 前四个区都是 `flex: 0 0 auto`，只有内容区 `flex: 1 1 auto; overflow-y: auto`。
 * **「开始 / 停止」固定放在底部操作条右端**，不跟任何卡片走——它曾经在实时页最下方的
 * 控制卡里，一出错误横幅就被挤出视口（900×620 溢出 22px、780×520 溢出 193px 且
 * 三层父级都是 overflow:visible，滚不到），而那时候恰恰最需要点它。
 */
type Tab = 'live' | 'quality' | 'keymap' | 'diagnostics' | 'setup'

const state = ref<AppState>({ kind: 'idle' })
const devices = ref<DeviceView[]>([])
const logs = ref<LogEntry[]>([])
const version = ref('—')
const logPath = ref('')
const tab = ref<Tab>('live')
const error = ref<AppError | null>(null)
const toast = ref('')

/** 错误条默认折叠成一行，点开才显示处理指引（展开会占 60px，默认不该占）。 */
const bannerOpen = ref(false)
/** 本次运行累计的 error 条数。诊断 tab 显示角标，进页面即清零。 */
const errorCount = ref(0)
/** 设备卡默认折叠：已连接时它只需一行 42px，省下来的高度全给电平表。 */
const deviceOpen = ref(false)
/** 指标默认只显三组核心项，其余收在「显示全部指标」里。 */
const moreMetrics = ref(false)

/**
 * 指标组是否收起。设备卡展开时自动置 true（见 toggleDevice），用户也可手动切。
 * 收起时不是消失——`metricBrief` 那一行仍然给三个最关键的数。
 */
const groupsCollapsed = ref(false)

const sinks = ref<SinkView[]>([])
const sink = ref<string | null>(null)
/** 这台机器上有没有能当麦克风用的输出设备（原生 16 kHz）。false = 还没装虚拟声卡。 */
const sinkAvailable = ref(true)
/** 内核自动挑中的那块声卡名，用于"一键选用"。 */
const sinkSuggested = ref<string | null>(null)
/** 声卡自检结果。null = 没跑过。 */
const sinkCheck = ref<SinkCheckReport | null>(null)
const sinkChecking = ref(false)
const targetMs = ref(80)
const saveWav = ref(false)
const autoConnect = ref(false)
/** 关麦兜底时长（分钟）。默认 10，可在设备展开区改。 */
const autoStopMin = ref(10)
const lastDevice = ref<string | null>(null)

const telemetry = ref<Telemetry | null>(null)
const summary = ref<SessionSummary | null>(null)
const lastSession = ref<SessionRecord | null>(null)
/** 设备电量百分比；null = 未连接或电量计不可用。设备只在变化时推送。 */
const battery = ref<number | null>(null)
/**
 * 是否已经收到过电量结论（无论有没有数字）。
 * 只靠 `battery === null` 分不清"还没读到"和"设备说不可用"：连上到读回结果之间有几十毫秒，
 * 那段时间不该闪一下"电量不可用"。
 */
const batteryKnown = ref(false)

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
const activeSessionId = computed(
  () => summary.value?.session_id ?? lastSession.value?.session_id ?? null,
)

/** 与固件 battery.c 的低电量阈值一致（APM_BATTERY_LOW_SOC_PCT = 20）。 */
const BATTERY_LOW_PCT = 20
const batteryLow = computed(() => battery.value !== null && battery.value <= BATTERY_LOW_PCT)
/**
 * 顶栏电量文案。`null` = 设备明确回报"电量不可用"（没有 Battery Service、读失败，
 * 或设备侧 SOC 无效——见 docs/KNOWN_ISSUES.md KI-007）。
 * 这时要**说出来**，不能藏起来：藏起来会让人以为功能没做。
 */
const batteryText = computed(() =>
  battery.value === null ? '电量不可用' : `电量 ${battery.value}%`,
)

const elapsedText = computed(() => {
  const ms = streaming.value ? (telemetry.value?.elapsed_ms ?? 0) : 0
  const total = Math.floor(ms / 1000)
  return `${String(Math.floor(total / 60)).padStart(2, '0')}:${String(total % 60).padStart(2, '0')}`
})

/**
 * 指标分三组，一组回答一个问题：链路有没有丢东西 / 声卡有没有出问题 / 延迟是多少。
 *
 * 口径全部写死在 docs/GUI.md §7，前端不自己算。分成三组不是为了好看：
 * 之前 12 项平铺在一行里，「缓冲」出现两次（buffer_ms 与 fill_p50）、
 * 顶栏还有一次，四个数字长得像但口径不同，用户没法判断哪个是延迟。
 */
interface MetricItem {
  label: string
  value: string
  tone?: 'ok' | 'warn'
}

const metricGroups = computed<{ title: string; items: MetricItem[] }[]>(() => {
  const t = telemetry.value
  if (!t) {
    return [
      { title: '链路', items: [{ label: '块', value: '—' }] },
      { title: '声卡', items: [{ label: '欠载', value: '—' }] },
      { title: '延迟', items: [{ label: '缓冲', value: '—' }, { label: '存量 p50/p95', value: '—' }] },
    ]
  }
  return [
    {
      title: '链路',
      items: [
        { label: '块', value: String(t.blocks) },
        { label: '丢块', value: String(t.lost_blocks), tone: t.lost_blocks > 0 ? 'warn' : 'ok' },
        { label: '跳变', value: String(t.seq_jumps), tone: t.seq_jumps > 0 ? 'warn' : 'ok' },
        {
          label: '设备丢弃',
          value: String(t.device_dropped),
          tone: t.device_dropped > 0 ? 'warn' : 'ok',
        },
      ],
    },
    {
      title: '声卡',
      items: [
        { label: '欠载', value: String(t.underruns) },
        { label: '断供', value: String(t.underruns_underfed) },
        { label: '静音', value: `${t.silent_frames} 帧` },
        { label: '锁定失败', value: String(t.lock_misses) },
        { label: '通道满', value: String(t.overflow_drops) },
      ],
    },
    {
      title: '延迟',
      items: [
        { label: '缓冲', value: `${t.buffer_ms.toFixed(0)} ms` },
        // 延迟用**实测存量**，不用目标值：修剪守卫会把稳态存量留在目标之上，
        // 拿目标当延迟会系统性低估（A4）。
        {
          label: '存量 p50/p95',
          value:
            t.fill_p50_ms == null || t.fill_p95_ms == null
              ? '—'
              : `${t.fill_p50_ms.toFixed(0)}/${t.fill_p95_ms.toFixed(0)}`,
        },
        { label: '漂移', value: `${t.ppm >= 0 ? '+' : ''}${t.ppm.toFixed(0)} ppm` },
      ],
    },
  ]
})

/** 折叠起来的次要指标。不是"不重要"，是不该跟核心项抢同一行。 */
const extraMetrics = computed<MetricItem[]>(() => {
  const t = telemetry.value
  if (!t) return []
  return [
    { label: '块长异常', value: String(t.bad_len_blocks) },
    { label: '毛刺事件', value: String(t.glitch_events) },
    { label: '用户基准', value: `${t.base_target_ms} ms` },
    { label: '自适应余量', value: `+${t.boost_ms} ms` },
    { label: '目标缓冲', value: `${t.target_ms} ms` },
    { label: '已写字节', value: t.bytes_written.toLocaleString('en-US') },
  ]
})

/**
 * 指标组收起时留的一行摘要：只给三个"要不要现在处理"的判断依据。
 *
 * 为什么不是干脆全藏起来：收起只是**视图**取舍，不是认为这些数不重要。
 * 推流中突然掉块或欠载暴涨，用户不展开也应该看得见。
 */
const metricBrief = computed<MetricItem[]>(() => {
  const t = telemetry.value
  if (!t) return []
  return [
    { label: '丢块', value: String(t.lost_blocks), tone: t.lost_blocks > 0 ? 'warn' : undefined },
    { label: '欠载', value: String(t.underruns) },
    { label: '缓冲', value: `${t.buffer_ms.toFixed(0)} ms` },
  ]
})

/**
 * 本场是否处于链路慢档（设备报 LINK_SLOW）。
 *
 * 慢档下设备只有 1.5–2 块/秒，声卡在狂补静音：界面上的欠载/漂移数字**全是假的**。
 * 必须显式说出来，否则用户会以为是声卡或漂移补偿出了问题（KI-005 / KI-006 的口径陷阱）。
 */
const linkSlow = computed(() => telemetry.value?.link_slow === true)

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
    bannerOpen.value = false
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
    // 内核已经按"虚拟声卡优先 + 稳定排序"挑过一块（`apm_sink::pick_usable_sink`），
    // 这里只跟着它的结论走：两份挑选逻辑各写一遍，就会在一台装了两块
    // 虚拟声卡的机器上互相抢。
    sinkAvailable.value = sinks.value.some((s) => s.supports_16k)
    if (!sink.value) {
      const preferred =
        (sinkSuggested.value && sinks.value.find((s) => s.name === sinkSuggested.value)) ||
        sinks.value.find((s) => s.supports_16k)
      if (preferred) {
        sink.value = preferred.name
        await api.setSink(preferred.name)
      }
    }
  })
}

/**
 * 一键验证当前选中的声卡：写一段已知电平的正弦，再从它的输入端回读。
 *
 * 为什么要给按钮而不是让用户"开个会试试"：会议软件里对方听不到，可能是
 * 写入、声卡直通、系统音量、会议软件选错设备四件事之一。这个按钮把前两件
 * 直接量出来，剩下的才轮到用户去查。
 */
async function runSinkCheck() {
  const name = sink.value
  if (!name || sinkChecking.value) return
  sinkChecking.value = true
  sinkCheck.value = null
  try {
    sinkCheck.value = await api.checkSink(name, 2)
    if (sinkCheck.value.verdict === 'pass') {
      flash('声卡直通正常，会议软件里把麦克风选成它即可')
    }
  } catch (err) {
    pushError(asAppError(err))
  } finally {
    sinkChecking.value = false
  }
}

/** 打开虚拟声卡下载页。不静默安装、不随包分发（BlackHole 是 GPL-3.0）。 */
async function openVirtualAudioDownload() {
  await handle(() => api.openVirtualAudioDownload())
}

/** 打开系统「声音」设置。路径由后端拼，前端只管点。 */
async function openSoundSettings() {
  await handle(() => api.openSoundSettings())
}

/** 声卡自检结论的界面文案。判定在后端做，这里只负责翻译。 */
const sinkCheckText = computed(() => {
  const r = sinkCheck.value
  if (!r) return ''
  const w = r.written_rms_dbfs === null ? '—' : `${r.written_rms_dbfs.toFixed(1)} dBFS`
  const l = r.loopback_rms_dbfs === null ? '—' : `${r.loopback_rms_dbfs.toFixed(1)} dBFS`
  switch (r.verdict) {
    case 'pass':
      return `直通正常：写入 ${w}，回读 ${l}`
    case 'loopback_silent':
      return `回读几乎静音（${l}）：要么这块声卡没有直通，要么本 App 没有麦克风权限`
    case 'loopback_no_samples':
      return '输入端一个样本都没读到：最可能是麦克风权限被拒绝'
    case 'sink_stalled':
      return `写入端自身有 ${r.underruns} 次欠载，回读结论不可采信（先解决喂入问题再验）`
  }
})

/** 声卡自检是否需要显示"没装虚拟声卡"的安装引导。 */
const sinkGuideOpen = ref(false)
const needSinkInstall = computed(() => !sinkAvailable.value && !streaming.value)

async function onSinkChange() {
  if (!sink.value) return
  const picked = sinks.value.find((s) => s.name === sink.value)
  if (picked && !picked.supports_16k) {
    // 先拦住再报：`Sink::open` 会拒绝，那时用户已经在等"开始"了。
    error.value = {
      code: 'sink_rate_unsupported',
      detail: `${picked.name} 不支持 16000 Hz 原生采样率；本项目不做隐式重采样`,
    }
    bannerOpen.value = false
  }
  await handle(() => api.setSink(sink.value as string))
}

async function onBufferChange() {
  await handle(() => api.setBufferTarget(targetMs.value))
}

/** 关麦兜底时长。改完立即生效（看门狗每轮现读配置），不用重开流。 */
async function onAutoStopChange() {
  await handle(() => api.setAutoStopMin(autoStopMin.value))
  flash(`连续采集 ${autoStopMin.value} 分钟会自动关麦`)
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

/** 设备卡的 DOM，展开时用它滚到可见（见 toggleDevice）。 */
const deviceCard = ref<HTMLElement | null>(null)

/**
 * 折叠 / 展开设备卡。
 *
 * 展开后卡片高约 281px，在默认窗口下内容区会开始滚动。这里做两件事：
 *
 * 1. **展开时自动收起指标组**，折叠时恢复用户原来的选择。
 *    不这么做的话，flex 只好把电平表压到 `min-height`（168px，曲线区很扁），
 *    而指标组仍然占着 100 多像素——用户点开设备是为了"换设备"，
 *    那一刻不看他刚才一直看着的链路/声卡/延迟三组数字。
 *    这是**视图状态**的自动调整，不改任何数据，退出展开即恢复。
 * 2. 展开后把卡片滚进可见区，"点了有反应"要立刻看得见。
 */
async function toggleDevice() {
  const next = !deviceOpen.value
  deviceOpen.value = next
  if (next) {
    groupsCollapsed.value = true
    await nextTick()
    deviceCard.value?.scrollIntoView({ block: 'nearest' })
  } else {
    groupsCollapsed.value = false
  }
}

function switchTab(next: Tab) {
  tab.value = next
  if (next === 'keymap') void loadKeymap()
  // 进诊断页即视为"已读"：角标统计的是本次运行新增的 error，
  // 原来的实现是扫 300 条滚动缓冲里的 error，一条就永久常亮，纯噪声。
  if (next === 'diagnostics') errorCount.value = 0
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
  // 不在诊断页时累加角标；在页面里就当作正在看，不累加。
  if (entry.level === 'error' && tab.value !== 'diagnostics') errorCount.value += 1
}

function pushError(e: AppError) {
  error.value = e
  bannerOpen.value = false
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
    autoStopMin.value = snap.settings.auto_stop_min
    lastDevice.value = snap.settings.last_device
    sink.value = snap.settings.sink
    // 电量先按快照回填：自动连接时后端可能在监听注册之前就读完了，
    // 只等事件会一直空白（顶栏的 v-if 要求 batteryKnown）。
    battery.value = snap.battery_pct
    batteryKnown.value = snap.battery_known
    sinkAvailable.value = snap.sink_available
    sinkSuggested.value = snap.sink_suggested
    // 没有上次连接记录 = 大概率是新装或新设备，直接落到烧写页，省掉"入口在哪"。
    // 已经连过设备的人不该被拽走，所以只在既没历史也没连接时切。
    if (
      !snap.settings.last_device &&
      snap.state.kind !== 'ready' &&
      snap.state.kind !== 'streaming'
    ) {
      tab.value = 'setup'
    }
    if (snap.state.kind === 'error') {
      pushError({ code: snap.state.code, detail: snap.state.detail })
    }
  } catch (err) {
    pushError(asAppError(err))
  }

  await refreshSinks()
  await loadKeymap()

  unlisten = await onAppEvent((event) => {
    if (event.type === 'state-changed') {
      state.value = event.state
      if (event.state.kind === 'error') {
        pushError({ code: event.state.code, detail: event.state.detail })
      } else if (event.state.kind === 'ready') {
        error.value = null
      }
      // 掉出 ready/streaming 就没有链路了，电量清零避免显示旧值。
      const kind = event.state.kind
      if (kind !== 'ready' && kind !== 'streaming') {
        battery.value = null
        batteryKnown.value = false
      }
    } else if (event.type === 'devices') {
      devices.value = event.devices
    } else if (event.type === 'event-log') {
      pushLog(event.entry)
    } else if (event.type === 'telemetry') {
      telemetry.value = event.telemetry
    } else if (event.type === 'session-done') {
      summary.value = event.summary
      // 会话结束就清掉实时指标：否则 linkSlow / 欠载这些数字会粘在界面上，
      // 下次开流前看着像"这一场也有问题"。
      telemetry.value = null
    } else if (event.type === 'battery') {
      battery.value = event.pct
      batteryKnown.value = true
    } else if (event.type === 'stream-aborted') {
      // 内核主动中止了推流（开流后迟迟没音频）。状态已经回 ready，用户能直接重试，
      // 所以这里只弹横幅说明原因，不占用状态机的 error 分支。
      pushError({ code: event.code, detail: event.detail })
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
    <!-- ============ 顶栏（固定 42px） ============
         只放"现在是什么状态"：连接态、设备、电量、信号、推流时长。
         版本号搬去诊断页底部、重新检测搬去设备展开区——低频操作不该占黄金位置。 -->
    <header class="topbar">
      <StatePill :state="state" />
      <div class="ident">
        <strong>{{ connectedDevice?.name ?? 'AI Passport Mic' }}</strong>
        <span v-if="connectedDevice" class="mono sub">{{ connectedDevice.id.slice(0, 8) }}…</span>
      </div>
      <span class="spacer" />
      <span
        v-if="connectedDevice && batteryKnown"
        class="meta"
        :class="{ low: batteryLow, na: battery === null }"
        :title="batteryLow ? '电量低于 20%，请充电' : undefined"
      >
        {{ batteryText }}{{ batteryLow ? ' · 请充电' : '' }}
      </span>
      <span v-if="connectedDevice?.rssi != null" class="meta mono">{{ connectedDevice.rssi }} dBm</span>
      <span v-if="streaming" class="meta mono" title="本场推流时长">{{ elapsedText }}</span>
    </header>

    <!-- ============ Tabs（固定 36px） ============
         顺序按使用频率：日常两个（实时/音质）在前，一次性操作（烧写）在最后。 -->
    <nav class="tabs">
      <button :class="{ active: tab === 'live' }" @click="switchTab('live')">实时</button>
      <button :class="{ active: tab === 'quality' }" @click="switchTab('quality')">音质报告</button>
      <button :class="{ active: tab === 'keymap' }" @click="switchTab('keymap')">按键映射</button>
      <button :class="{ active: tab === 'diagnostics' }" @click="switchTab('diagnostics')">
        诊断
        <i v-if="errorCount > 0" class="count" :title="`本次运行新增 ${errorCount} 条 error`">
          {{ errorCount > 9 ? '9+' : errorCount }}
        </i>
      </button>
      <button :class="{ active: tab === 'setup' }" @click="switchTab('setup')">设备烧写</button>
      <span class="spacer" />
      <span v-if="stateKind === 'no_adapter'" class="hint">没有可用蓝牙适配器</span>
    </nav>

    <!-- ============ 错误条（固定 34px / 展开再加） ============
         单行摘要 + 点开展开处理指引。原来是一次性铺开 60px，
         在默认窗口下会把底部操作条顶出屏幕。 -->
    <template v-if="error">
      <div class="banner" @click="bannerOpen = !bannerOpen">
        <span class="code mono">{{ error.code }}</span>
        <span class="msg">{{ error.detail }}</span>
        <span class="spacer" />
        <span class="act">点此<b>{{ bannerOpen ? '收起' : '展开详情' }}</b></span>
        <button class="sm ghost" @click.stop="error = null">关闭</button>
      </div>
      <div v-if="bannerOpen" class="banner-detail">
        <strong>怎么处理：</strong>{{ errorHint }}
        <span class="dim">（错误码与完整证据链见「诊断」页）</span>
      </div>
    </template>

    <!-- ============ 内容区（唯一滚动） ============ -->
    <main class="content scroll">
      <!-- ---------- 实时 ---------- -->
      <div v-if="tab === 'live'" class="page live">
        <!-- 虚拟声卡引导：**必须顶在最前面**。macOS 自带扬声器不接受 16 kHz，
             没有虚拟声卡时"开始"一定失败，而且失败信息（sink_missing）
             对用户毫无指向性。这条卡片只在真的没有可用设备时出现。 -->
        <section v-if="needSinkInstall" class="card sink-guide">
          <div class="card-head">
            <h2>还差一步：装一块虚拟声卡</h2>
            <span class="spacer" />
            <button class="sm ghost" @click="sinkGuideOpen = !sinkGuideOpen">
              {{ sinkGuideOpen ? '收起' : '怎么做' }}
            </button>
          </div>
          <p>
            这台电脑上还没有能把音频"变成麦克风"的设备。会议软件只能选系统里已有的
            输入设备，所以需要一个虚拟声卡当中转：
            <b>本软件 → 虚拟声卡 → 会议软件</b>。
          </p>
          <p class="dim">
            已装过但这里还是空白？点底栏的「重新枚举」，或确认它是原生支持 16 kHz 的
            那一款（BlackHole 2ch 可以，16ch 也可以）。
          </p>
          <div v-if="sinkGuideOpen" class="sink-guide-body">
            <ol>
              <li>
                打开
                <button class="link" @click="openVirtualAudioDownload">BlackHole 下载页</button>
                （免费，macOS 13+）。
              </li>
              <li>安装时选择 <code>2ch</code>；装完可能需要重启，或执行 <code>killall coreaudiod</code>。</li>
              <li>回到本窗口点「重新枚举」，下拉框里选中新出现的 <code>BlackHole 2ch</code>。</li>
              <li>
                点底栏「验证声卡」跑一次自检——
                <b>它会把"写进去"和"读回来"的电平都量出来</b>，通过再去开会。
              </li>
              <li>
                最后在会议软件里把麦克风选成 <code>BlackHole 2ch</code> 的
                <b>输入</b>。选错方向是这一步最常见的坑。
              </li>
            </ol>
          </div>
        </section>

        <section class="card meter grow-card">
          <div class="card-head">
            <h2>电平与指标</h2>
            <span class="spacer" />
            <span
              v-if="linkSlow"
              class="tag warn"
              title="设备报 LINK_SLOW：连接间隔 > 40 ms，撑不住 50 块/秒"
            >
              链路慢档
            </span>
            <span class="tag" :class="{ live: streaming }">{{ streaming ? 'LIVE' : '待机' }}</span>
          </div>

          <p v-if="linkSlow" class="slow-note">
            设备报 <code>LINK_SLOW</code>（连接间隔 &gt; 40 ms），撑不住 50 块/秒。
            <strong>下面的欠载与漂移是链路假象</strong>，不能当音质结论；本场也不会出音质报告。
            复位设备后重连再试。
          </p>

          <div class="meter-wrap">
            <LevelMeter
              :rms-dbfs="telemetry?.rms_dbfs ?? null"
              :peak-dbfs="telemetry?.peak_dbfs ?? null"
              :streaming="streaming"
            />
          </div>

          <!-- 指标区：正常态是三组；设备卡展开时降级为一行摘要（见 groupsCollapsed）。
               降级的理由不是"这些数不重要"，而是那一刻用户在换设备、不在这里读指标，
               把高度让给电平表比把电平表压到 168px 更好。摘要仍给三个判断依据。 -->
          <div v-if="!groupsCollapsed" class="groups">
            <div v-for="g in metricGroups" :key="g.title" class="group">
              <div class="group-head">{{ g.title }}</div>
              <div class="kvs">
                <span v-for="m in g.items" :key="m.label" class="kv">
                  <i>{{ m.label }}</i>
                  <b :class="m.tone">{{ m.value }}</b>
                </span>
              </div>
            </div>
          </div>
          <div v-else class="kvs brief-row">
            <span v-for="m in metricBrief" :key="m.label" class="kv">
              <i>{{ m.label }}</i>
              <b :class="m.tone">{{ m.value }}</b>
            </span>
            <button class="more-toggle inline-toggle" @click="groupsCollapsed = false">
              展开指标
            </button>
          </div>

          <template v-if="!groupsCollapsed">
            <div v-if="moreMetrics && extraMetrics.length" class="kvs extra">
              <span v-for="m in extraMetrics" :key="m.label" class="kv">
                <i>{{ m.label }}</i>
                <b>{{ m.value }}</b>
              </span>
            </div>
            <button class="more-toggle" @click="moreMetrics = !moreMetrics">
              {{ moreMetrics ? '收起多余指标' : '显示全部指标' }}
            </button>
          </template>
        </section>

        <!-- 设备卡默认折叠成一行；展开才占 280px -->
        <section ref="deviceCard" class="card device">
          <div
            class="device-row"
            :data-open="deviceOpen"
            @click="toggleDevice"
          >
            <i class="chev" :data-open="deviceOpen" />
            <span class="dlabel">设备</span>
            <span class="dname">{{ connectedDevice?.name ?? lastDevice ?? '未连接' }}</span>
            <span v-if="connectedDevice" class="dstate">已连接</span>
            <span v-else-if="scanning" class="dstate busy">扫描中…</span>
            <span v-if="connectedDevice?.rssi != null" class="dm mono">{{ connectedDevice.rssi }} dBm</span>
            <span class="spacer" />
            <button
              v-if="connectedDevice"
              class="sm danger"
              :disabled="streaming"
              @click.stop="disconnect"
            >
              断开
            </button>
            <button v-else class="sm primary" :disabled="!canScan" @click.stop="scan">扫描</button>
          </div>

          <div v-if="deviceOpen" class="device-body">
            <div class="device-actions">
              <button v-if="scanning" class="sm" @click="stopScan">停止扫描</button>
              <button v-else class="sm" :disabled="!canScan" @click="scan">扫描附近设备</button>
              <label class="check" title="启动后自动连接上次的设备（设备需已上电并在广播）">
                <input v-model="autoConnect" type="checkbox" @change="onAutoConnectChange" />
                启动时自动连接
              </label>
              <span class="spacer" />
              <button class="sm ghost" :disabled="busy" @click="detect">重新检测适配器</button>
            </div>

            <DeviceList
              :devices="devices"
              :connected-id="connectedDevice?.id ?? null"
              :busy="busy"
              :scanning="scanning"
              @connect="connect"
            />
            <p v-if="stateKind === 'idle' && lastDevice" class="note">
              上次连接：{{ lastDevice }}{{ autoConnect ? '，启动时自动连接' : '' }}
            </p>

            <div class="hr" />
            <div class="capture-row">
              <span class="note">关麦兜底</span>
              <input
                v-model.number="autoStopMin"
                type="number"
                min="1"
                max="1440"
                step="1"
                class="num mono"
                title="连续采集这么久就自动关麦，防忘记。所有推流都生效"
                @change="onAutoStopChange"
              />
              <span class="note">分钟</span>
              <span class="note dim">连续采集这么久自动关麦（1–1440），改完立即生效</span>
            </div>
          </div>
        </section>
      </div>

      <!-- ---------- 音质报告 ---------- -->
      <div v-else-if="tab === 'quality'" class="page">
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
      </div>

      <!-- ---------- 按键映射 ---------- -->
      <div v-else-if="tab === 'keymap'" class="page">
        <KeymapView
          :view="keymap"
          :busy="keymapBusy"
          :error="keymapError"
          @save="saveKeymap"
          @reset="resetKeymap"
          @import="importKeymap"
          @copy="copyKeymap"
        />
      </div>

      <!-- ---------- 诊断 ---------- -->
      <div v-else-if="tab === 'diagnostics'" class="page">
        <DiagnosticsView
          :entries="logs"
          :log-path="logPath"
          :version="version"
          @copy="copyDiagnostics"
          @reveal="revealLog"
        />
      </div>

      <!-- ---------- 设备烧写 ---------- -->
      <div v-else class="page">
        <FirmwareView />
      </div>
    </main>

    <!-- ============ 底部操作条（固定 56px） ============
         声卡 / 缓冲 / 保存 WAV / 开始停止。这四个是"每次开流都要碰"的东西，
         放这里的好处是位置永不变化：错误横幅、慢档提示、窗口缩小都挤不掉它。 -->
    <footer class="dock">
      <span class="fl">声卡</span>
      <select
        v-model="sink"
        :disabled="streaming"
        class="sink-select"
        title="推流目标声卡（会议软件里要把麦克风选成它）"
        @change="onSinkChange"
      >
        <option :value="null" disabled>选择输出设备…</option>
        <option v-for="s in sinks" :key="s.name" :value="s.name">
          {{ s.name }}{{ s.supports_16k ? '' : '（不支持 16 kHz）' }}
        </option>
      </select>
      <button class="sm ghost" :disabled="streaming" title="装完虚拟声卡后点这里" @click="refreshSinks">
        重新枚举
      </button>
      <button
        class="sm ghost"
        :disabled="streaming || !sink || sinkChecking"
        :title="
          sink
            ? '往这块声卡写一段测试音，再从它的输入端回读，验证直通是否正常'
            : '先选一块声卡'
        "
        @click="runSinkCheck"
      >
        {{ sinkChecking ? '验证中…' : '验证声卡' }}
      </button>

      <span class="divider" />

      <span class="fl">缓冲</span>
      <input
        v-model.number="targetMs"
        type="range"
        min="40"
        max="80"
        step="5"
        title="用户基准，自适应不会改它；40–80 ms 是延迟预算决定的"
        @change="onBufferChange"
      />
      <span class="val mono">{{ targetMs }} ms</span>

      <label class="check" title="把这一场录成 WAV，停止时可出音质报告">
        <input v-model="saveWav" type="checkbox" :disabled="streaming" @change="onSaveWavChange" />
        保存 WAV
      </label>

      <span class="spacer" />

      <button v-if="!streaming" class="primary main" :disabled="!canStart" @click="startStream">
        开始
      </button>
      <button v-else class="danger main" @click="stopStream">停止</button>
    </footer>

    <!-- 声卡自检结果：显示在底栏上方，不挡住操作条。
         用 `aria-live` 让读屏器念出来——这个结论只有视觉呈现的话，
         盲用户完全拿不到"声卡到底通没通"。 -->
    <div v-if="sinkCheck" class="sink-check" :class="sinkCheck.verdict" aria-live="polite">
      <span class="verdict">{{ sinkCheck.verdict === 'pass' ? 'PASS' : 'FAIL' }}</span>
      <span class="text">{{ sinkCheckText }}</span>
      <span class="spacer" />
      <span class="dim mono">{{ sinkCheck.device_name }}</span>
      <button class="sm ghost" @click="openSoundSettings">系统声音设置</button>
      <button class="sm ghost" @click="sinkCheck = null">关闭</button>
    </div>

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
  min-height: 0;
  overflow: hidden;
}

/* ---------- 顶栏 ---------- */
.topbar {
  flex: 0 0 auto;
  height: var(--h-topbar);
  display: flex;
  align-items: center;
  gap: var(--s2);
  padding: 0 var(--s4);
  border-bottom: 1px solid var(--line-soft);
}

.ident {
  display: flex;
  align-items: baseline;
  gap: 6px;
  min-width: 0;
}

.ident strong {
  font-size: var(--fs-13);
  font-weight: 600;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.sub {
  color: var(--dim);
  font-size: var(--fs-11);
}

.meta {
  font-size: var(--fs-11);
  color: var(--muted);
  white-space: nowrap;
  font-variant-numeric: tabular-nums;
}

.meta.low {
  color: var(--warn);
  font-weight: 600;
}

/* 电量不可用：低调但要说出来，不能用警告色制造"有故障"的错觉。 */
.meta.na {
  color: var(--dim);
}

/* ---------- Tabs ---------- */
.tabs {
  flex: 0 0 auto;
  display: flex;
  align-items: stretch;
  gap: 2px;
  height: var(--h-tabs);
  padding: 0 var(--s4);
  border-bottom: 1px solid var(--line-soft);
}

.tabs button {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  border: none;
  background: transparent;
  border-radius: 0;
  padding: 0 10px;
  color: var(--muted);
  border-bottom: 2px solid transparent;
  margin-bottom: -1px;
}

.tabs button:hover:not(:disabled) {
  color: var(--text);
  background: transparent;
  border-color: transparent;
  border-bottom-color: #ffffff14;
}

.tabs button.active {
  color: var(--text);
  border-bottom-color: var(--accent);
}

.count {
  min-width: 15px;
  height: 15px;
  padding: 0 4px;
  border-radius: var(--r-pill);
  background: #ff5f5f28;
  color: var(--err);
  font-size: var(--fs-10);
  font-style: normal;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}

.hint {
  align-self: center;
  font-size: var(--fs-11);
  color: var(--dim);
}

/* ---------- 错误条 ---------- */
.banner {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: var(--s2);
  height: 34px;
  padding: 0 var(--s4);
  background: #ff5f5f0d;
  border-bottom: 1px solid #ff5f5f26;
  cursor: pointer;
  min-width: 0;
}

.banner .code {
  color: var(--err);
  font-weight: 600;
  white-space: nowrap;
}

.banner .msg {
  font-size: var(--fs-11);
  color: #e6c9c9;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
}

.banner .act {
  font-size: var(--fs-11);
  color: var(--muted);
  white-space: nowrap;
}

.banner .act b {
  color: var(--text);
  font-weight: 500;
}

.banner-detail {
  flex: 0 0 auto;
  padding: var(--s2) var(--s4);
  background: #ff5f5f08;
  border-bottom: 1px solid #ff5f5f26;
  font-size: var(--fs-11);
  line-height: 1.65;
  color: var(--muted);
}

.banner-detail strong {
  color: var(--text);
  font-weight: 500;
}

.banner-detail .dim {
  color: var(--dim);
}

/* ---------- 内容区（唯一滚动） ---------- */
.content {
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  overflow-x: hidden;
  padding: var(--s3) var(--s4);
  display: flex;
  flex-direction: column;
  gap: var(--gap-card);
}

.page {
  display: flex;
  flex-direction: column;
  gap: var(--gap-card);
  /* `1 0 auto` 而不是 `1 1 auto`：页面可以**长高**，但不允许被**压缩**。
     空间不够时（典型：设备卡展开、或出现错误条）应当由 .content 滚动，
     而不是把里面 flex:1 的电平卡压到内容高度以下——那会让 .groups 与
     「显示全部指标」溢出卡片、直接盖在设备卡上（2026-11-07 修）。
     注意 `min-height: 0` 会抵消这个保护，所以这里不写它。 */
  flex: 1 0 auto;
}

/* ---------- 实时页 ---------- */
.card.meter {
  display: flex;
  flex-direction: column;
  gap: var(--s3);
  min-height: 0;
}

.grow-card {
  flex: 1 1 auto;
}

.card-head {
  display: flex;
  align-items: center;
  gap: var(--s2);
  min-height: 16px;
}

.slow-note {
  margin: 0;
  font-size: var(--fs-11);
  line-height: 1.6;
  color: var(--warn);
}

.slow-note code {
  background: #ffb0201a;
  padding: 0 3px;
  border-radius: 3px;
}

/* 电平表的盒子。
 *
 * 两条约束都是必须的，不是风格选择：
 *
 * 1. `position: relative` + 子元素 absolute。canvas 的 `height: 100%` 在"高度未定"
 *    的父级上会退化成 auto，于是采用自身 `height` 属性；而 LevelMeter 每帧又把属性
 *    设成 `clientHeight`——两者互相喂养，卡片会自己长高（实测闭合态被撑到 591px，
 *    比可视区还高）。absolute 之后 canvas 完全不参与父级尺寸计算。
 * 2. 盒子自身不设 `height`，只设 `min-height` 兜底；可用高度由 flex 给。
 *    这样"设备卡展开 → 可用高度变小 → 电平表变矮"是单向的，不会反向撑高页面。
 */
.meter-wrap {
  position: relative;
  flex: 1 1 auto;
  min-height: 168px;
  border-radius: var(--r-sm);
  overflow: hidden;
  background: var(--panel-2);
}

.meter-wrap > :deep(canvas) {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
}

/* ---------- 指标三组 ---------- */
.groups {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: var(--s3);
  border-top: 1px solid var(--line-soft);
  padding-top: var(--s3);
}

.group {
  display: flex;
  flex-direction: column;
  gap: 6px;
  min-width: 0;
}

.group-head {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: var(--fs-10);
  letter-spacing: 0.06em;
  color: var(--dim);
  text-transform: uppercase;
}

.group-head::after {
  content: '';
  flex: 1;
  height: 1px;
  background: var(--line-soft);
}

.kvs {
  display: flex;
  flex-wrap: wrap;
  gap: 3px 14px;
}

.kv {
  display: inline-flex;
  align-items: baseline;
  gap: 5px;
  min-width: 0;
}

.kv i {
  font-size: var(--fs-11);
  font-style: normal;
  color: var(--dim);
  white-space: nowrap;
}

.kv b {
  font-family: var(--mono);
  font-size: var(--fs-12);
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}

.kv b.ok {
  color: var(--ok);
}

.kv b.warn {
  color: var(--warn);
}

.kvs.extra {
  border-top: 1px solid var(--line-soft);
  padding-top: var(--s2);
}

/* 设备卡展开时的降级指标行：一行三个数 + 一个展开入口，高度只有一组的三分之一。 */
.kvs.brief-row {
  align-items: center;
  gap: 4px 14px;
  border-top: 1px solid var(--line-soft);
  padding-top: var(--s2);
}

.inline-toggle {
  margin-left: auto;
}

/* 「显示全部指标」是个零 padding 的文本按钮，贴在卡片底边时视觉上像压着下一张卡。
   给一点下留白，让卡片底边有自己的呼吸空间。 */
.more-toggle {
  align-self: flex-start;
  border: none;
  background: transparent;
  padding: 0;
  font-size: var(--fs-11);
  color: var(--dim);
  line-height: 1.5;
}

.more-toggle.inline-toggle {
  align-self: center;
  margin-left: auto;
}

.more-toggle:hover:not(:disabled) {
  color: var(--muted);
  background: transparent;
  border-color: transparent;
}

/* ---------- 设备（默认折叠） ---------- */
.card.device {
  padding: 0;
  overflow: hidden;
  flex: 0 0 auto;
}

/* 标题行与展开区之间有一条分隔线，让"一行摘要"和"展开的详情"在视觉上分开。 */
.device-row {
  display: flex;
  align-items: center;
  gap: var(--s2);
  height: 42px;
  padding: 0 var(--s3);
  cursor: pointer;
  min-width: 0;
}

.device-row[data-open='true'] {
  border-bottom: 1px solid var(--line-soft);
}

.device-row:hover {
  background: #ffffff04;
}

.chev {
  width: 8px;
  height: 8px;
  flex: 0 0 auto;
  border-right: 1.5px solid var(--muted);
  border-bottom: 1.5px solid var(--muted);
  transform: rotate(-45deg);
  transition: transform 0.16s;
  margin-left: 2px;
}

.chev[data-open='true'] {
  transform: rotate(45deg);
}

.dlabel {
  font-size: var(--fs-10);
  letter-spacing: 0.06em;
  text-transform: uppercase;
  color: var(--dim);
  flex: 0 0 auto;
}

.dname {
  font-size: var(--fs-12);
  font-weight: 500;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.dstate {
  font-size: var(--fs-11);
  color: var(--ok);
  white-space: nowrap;
}

.dstate.busy {
  color: var(--accent);
}

.dm {
  font-size: var(--fs-11);
  color: var(--dim);
  white-space: nowrap;
}

/* 展开区：与上方标题行之间要留白。`padding-top` 不能是 0，否则
   「扫描附近设备」那一行会紧贴标题行的下边框（标题行自己有 42px 固定高度，
   视觉上会把两行粘成一块）。 */
.device-body {
  padding: var(--s3) var(--s3) var(--s3);
  display: flex;
  flex-direction: column;
  gap: var(--s2);
}

/* 设备列表是一个独立区块（可以滚动、有 hover 行），和上面的操作行不是一组。
   这里给它一点上留白，否则「扫描附近设备」那一行的按钮会贴着第一行设备的边框。 */
.device-body > :deep(.list) {
  margin-top: var(--s1);
}

.device-actions {
  display: flex;
  align-items: center;
  gap: var(--s2);
  flex-wrap: wrap;
}

.check {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: var(--fs-11);
  color: var(--muted);
  white-space: nowrap;
  cursor: pointer;
}

.hr {
  height: 1px;
  background: var(--line-soft);
}

.capture-row {
  display: flex;
  align-items: center;
  gap: var(--s2);
  flex-wrap: wrap;
}

.num {
  width: 58px;
  padding: 3px 6px;
  text-align: right;
}

.dim {
  color: var(--dim);
}

/* ---------- 底部操作条 ---------- */
.dock {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: var(--s2);
  height: var(--h-dock);
  padding: 0 var(--s4);
  background: var(--panel);
  border-top: 1px solid var(--line);
}

.dock .fl {
  font-size: var(--fs-11);
  color: var(--dim);
  white-space: nowrap;
}

.sink-select {
  min-width: 160px;
  max-width: 210px;
}

.dock input[type='range'] {
  width: 104px;
  padding: 0;
  border: none;
  background: transparent;
  accent-color: var(--accent);
  cursor: pointer;
}

.dock .val {
  color: var(--muted);
  min-width: 46px;
  font-variant-numeric: tabular-nums;
}

.divider {
  width: 1px;
  height: 20px;
  background: var(--line);
  margin: 0 2px;
}

.dock .main {
  min-width: 92px;
  height: 32px;
  font-size: var(--fs-13);
  padding: 0 18px;
}

/* ---------- 虚拟声卡引导 ----------
   这条卡片出现时用户一定还没装虚拟声卡，"开始"必然失败。
   所以它必须有视觉重量（不跟普通卡片同色），否则会被当成提示语划过去。 */
.sink-guide {
  border-color: #4a6b8a;
  background: linear-gradient(180deg, #16283a, #131a22);
}

.sink-guide .card-head h2 {
  color: #ffd479;
}

.sink-guide p {
  margin: 6px 0 0;
  font-size: var(--fs-12);
  line-height: 1.65;
  color: var(--muted);
}

.sink-guide-body {
  margin-top: var(--s3);
  padding-top: var(--s3);
  border-top: 1px solid var(--line-soft);
}

.sink-guide-body ol {
  margin: 0;
  padding-left: 20px;
  font-size: var(--fs-12);
  line-height: 1.9;
  color: var(--muted);
}

.sink-guide-body code {
  padding: 1px 5px;
  border-radius: 3px;
  background: #ffffff0e;
  font-size: var(--fs-11);
}

/* 行内按钮：外观是链接，但用的是 button —— 它执行动作（打开浏览器），
   不是跳转，用 <a> 会让键盘用户以为能用 Enter 在新标签打开。 */
button.link {
  display: inline;
  padding: 0;
  border: none;
  background: none;
  color: var(--accent);
  font-size: inherit;
  text-decoration: underline;
  cursor: pointer;
}

/* ---------- 声卡自检结果条 ----------
   放在底栏上方而不是内容区：验证时用户可能在任何 tab，
   结论必须跟着走，且不能把底栏的"开始"挤偏。 */
.sink-check {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: var(--s2);
  padding: 6px var(--s4);
  font-size: var(--fs-12);
  border-top: 1px solid var(--line-soft);
}

.sink-check.pass {
  background: #16301f;
  color: #b9e6c6;
}

.sink-check.fail {
  background: #33201f;
  color: #f0c0bc;
}

.sink-check .verdict {
  font-weight: 700;
  font-size: var(--fs-11);
  letter-spacing: 0.04em;
}

.sink-check .text {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* ---------- toast ---------- */
.toast {
  position: fixed;
  right: 18px;
  bottom: 68px;
  background: var(--panel-3);
  border: 1px solid #2f3d4e;
  color: var(--text);
  padding: 7px 14px;
  border-radius: var(--r-sm);
  font-size: var(--fs-12);
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
