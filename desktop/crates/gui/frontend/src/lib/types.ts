// 与 Rust 侧 IPC 契约一一对应（docs/GUI.md §6）。改这里必须同步改
// desktop/crates/app/src/{state.rs,events.rs,session.rs,report.rs,keymap_store.rs}。

export type ErrorCode =
  | 'bt_unauthorized'
  | 'bt_unavailable'
  | 'device_not_found'
  | 'connect_timeout'
  | 'subscribe_failed'
  | 'sink_missing'
  | 'sink_rate_unsupported'
  | 'protocol_mismatch'
  | 'keymap_invalid'
  | 'flash_no_device'
  | 'firmware_image_invalid'
  | 'flash_failed'
  | 'flash_verify_failed'
  | 'capture_not_starting'
  | 'busy'
  | 'internal'

export interface AppError {
  code: ErrorCode
  detail: string
}

// ---- M6：固件烧写（对应 `apm_app::firmware`） ----

export interface ImageInfo {
  path: string
  version: string
  bytes: number
  md5: string
}

export interface PortView {
  path: string
  serial_number: string | null
}

/** 开箱页首屏：随包镜像 + 候选串口。不碰设备。 */
export interface FirmwareStatus {
  image: ImageInfo | null
  /** 镜像缺失/损坏的原因。有值时不允许点"烧写"。 */
  image_error: string | null
  ports: PortView[]
}

export type DeviceFirmwareState = 'up_to_date' | 'blank' | 'mismatch' | 'unknown'

export interface ProbeResult {
  port: string
  chip: string
  current_version: string | null
  image_version: string
  state: DeviceFirmwareState
  /** 空片/读不到 → 首次烧写，界面换文案。 */
  first_flash: boolean
}

export interface FlashReport {
  port: string
  from_version: string | null
  to_version: string
  bytes_written: number
  md5: string
  verified: boolean
  elapsed_ms: number
}

export interface DeviceView {
  id: string
  name: string
  rssi: number | null
  has_service: boolean
  connected: boolean
}

export type AppState =
  | { kind: 'no_adapter' }
  | { kind: 'idle' }
  | { kind: 'scanning'; window_ms: number }
  | { kind: 'connecting'; device_id: string }
  | { kind: 'ready'; device: DeviceView }
  | { kind: 'streaming'; device: DeviceView; session_id: string }
  | { kind: 'stopping' }
  | { kind: 'error'; code: ErrorCode; detail: string }

export type StateKind = AppState['kind']

export type LogLevel = 'debug' | 'info' | 'warn' | 'error'

export interface LogEntry {
  ts_ms: number
  level: LogLevel
  source: string
  message: string
}

export interface SettingsView {
  sink: string | null
  target_ms: number
  save_wav: boolean
  /** 启动后自动连接上次设备。 */
  auto_connect: boolean
  /** 上次连接的设备名（无记录为 null），只用于提示。 */
  last_device: string | null
  /** 连续采集多少分钟后自动停（关麦兜底）。所有推流都生效。 */
  auto_stop_min: number
}

export interface SessionRecord {
  session_id: string
  wav_path: string | null
  started_ms: number
}

export interface AppSnapshot {
  version: string
  state: AppState
  devices: DeviceView[]
  log_tail: LogEntry[]
  log_path: string
  log_dir: string
  settings: SettingsView
  last_session: SessionRecord | null
  /** 电量百分比（0..100）。`battery_known=false` 时无意义。 */
  battery_pct: number | null
  /**
   * 是否已经拿到过电量结论（无论有没有数字）。
   * 自动连接时 `Battery` 事件可能早于监听注册而丢失，靠这个字段补一次，
   * 否则顶栏会一直空白——不是没做，是没收到。
   */
  battery_known: boolean
}

/** 5 Hz 遥测。字段口径见 docs/GUI.md §7，前端**不自己算**丢块率。 */
export interface Telemetry {
  rms_dbfs: number | null
  peak_dbfs: number | null
  buffer_ms: number
  /** 实测存量中位数（ms，整场累计）。延迟结论应当引它，而不是 target_ms。 */
  fill_p50_ms: number | null
  /** 实测存量 p95（ms）。延迟预算里"抖动缓冲"那一项用它。 */
  fill_p95_ms: number | null
  /** 当前实际生效的目标（基准 + 自适应余量）。 */
  target_ms: number
  /** 用户选的基准目标；自适应不会改它。 */
  base_target_ms: number
  /** 自适应临时余量（ms）。非 0 说明最近发生过欠载。 */
  boost_ms: number
  blocks: number
  lost_blocks: number
  seq_jumps: number
  device_dropped: number
  underruns: number
  /** 其中上游断供造成的次数。与 underruns 分开：断供型调缓冲无效。 */
  underruns_underfed: number
  silent_frames: number
  lock_misses: number
  bytes_written: number
  elapsed_ms: number
  ppm: number
  glitch_events: number
  overflow_drops: number
  bad_len_blocks: number
  /** 设备报过 LINK_SLOW：本场欠载/漂移是链路假象，不能当音质结论。 */
  link_slow: boolean
}

export interface SessionSummary {
  session_id: string
  blocks: number
  frames: number
  duration_s: number
  sink_device: string | null
  wav_path: string | null
  bytes_written: number
  underruns: number
  /** 其中上游断供（距上次推入 > 30 ms）造成的次数。 */
  underruns_underfed: number
  silent_frames: number
  frames_dropped: number
  /** 延迟守卫丢弃的最旧帧数（回调停摆积压），与 frames_dropped 分开计。 */
  frames_trimmed: number
  lock_misses: number
  ppm: number
  frames_inserted: number
  frames_skipped: number
  /** 会话开始时用户选的缓冲基准（ms）。自适应不改它。 */
  target_start_ms: number
  /** 会话结束时用户选的基准（运行中改过滑块就会变）。 */
  target_end_ms: number
  /** 会话期间自适应最多临时加过多少余量（ms）。0 = 全程没被抬过。 */
  target_boost_max_ms: number
  /** 实测存量中位数（ms）。报告里的"抖动缓冲延迟"用它。 */
  buffer_p50_ms: number | null
  /** 实测存量 p95（ms）。 */
  buffer_p95_ms: number | null
  glitch_events: number
  glitch_muted_ms: number
  lost_blocks: number
  seq_jumps: number
  device_dropped: number
  bad_len_blocks: number
  overflow_drops: number
  /** 设备报过 LINK_SLOW：本场数字是链路假象，音质报告会拒绝出结论。 */
  link_slow: boolean
}

export interface SinkView {
  name: string
  /** 支持范围只用于"不支持 16 kHz"的错误文案。 */
  min_rate: number
  max_rate: number
  supports_16k: boolean
}

export interface ReportCheck {
  name: string
  ok: boolean
  actual: string
  expected: string
}

/** 与 `apmctl analyze --json` 同源（apm_app::report）。未测字段是 null，不是 0。 */
export interface QualityReport {
  recording: string
  sample_rate: number
  channels: number
  downmixed: boolean
  duration_s: number
  samples: number
  rms_dbfs: number | null
  peak_dbfs: number | null
  clipping_pct: number
  dc_offset_pct: number
  zero_crossing_pct: number
  glitch_events: number
  glitch_ms: number
  /** 采样率不是 16 kHz 时的提醒，与 apmctl 同一份文案；16 kHz 时为 null。 */
  rate_note: string | null
  checks: ReportCheck[]
  pass: boolean
  verdict: string
}

export interface BindingView {
  event: string
  modifiers: string[]
  key: string
  repeat: number
  is_ptt: boolean
  locked: boolean
}

export interface BindingInput {
  event: string
  modifiers: string[]
  key: string
  repeat: number
}

export interface KeymapView {
  path: string
  schema: string
  bindings: BindingView[]
  error: string | null
}

export type AppEvent =
  | { type: 'state-changed'; state: AppState }
  | { type: 'devices'; devices: DeviceView[] }
  | { type: 'event-log'; entry: LogEntry }
  | { type: 'telemetry'; telemetry: Telemetry }
  | { type: 'session-done'; summary: SessionSummary }
  | { type: 'battery'; pct: number | null }
  /** 推流被内核主动中止（目前只有"开流后迟迟没音频"这一种）。状态仍回到 ready，可重试。 */
  | { type: 'stream-aborted'; code: ErrorCode; detail: string }

export const STATE_LABEL: Record<StateKind, string> = {
  no_adapter: '无蓝牙适配器',
  idle: '待连接',
  scanning: '扫描中',
  connecting: '连接中',
  ready: '已连接',
  streaming: '推流中',
  stopping: '收尾中',
  error: '出错',
}

/** 与 Rust 的 ErrorCode::hint 对应：前端只负责展示，不重新判断原因。 */
export const ERROR_HINTS: Record<ErrorCode, string> = {
  bt_unauthorized: '系统设置 → 隐私与安全性 → 蓝牙，允许本 App 后点「重新检测」',
  bt_unavailable: '确认系统蓝牙已打开；虚拟机与远程会话通常没有可用适配器',
  device_not_found: '确认设备已上电并在广播（开机后 3 秒内开始广播）',
  connect_timeout: '设备可能已被其他主机占用，靠近一点再试',
  subscribe_failed: '重连一次；连续失败请重启设备',
  sink_missing: '选定的虚拟声卡不见了，检查 BlackHole 是否已加载',
  sink_rate_unsupported: '该声卡不支持 16 kHz，换一个原生支持 16 kHz 的输出',
  protocol_mismatch: '固件与桌面端版本不一致，重新烧录同一版本',
  keymap_invalid: '键名或修饰键不在支持列表内；PTT 的停止通道不能删',
  flash_no_device:
    '用数据线直连电脑（不要经无源 HUB），确认设备已上电，再换个 USB 口试一次',
  firmware_image_invalid: '安装包里的固件损坏或缺失，重新安装本软件',
  flash_failed: '重插设备后重试；反复失败可以按住 BOOT 键再插线（强制进下载模式）',
  flash_verify_failed: '写入内容和镜像对不上，设备大概率还在下载模式，直接重试一次',
  capture_not_starting:
    '设备没开始发送音频（链路慢档或半死态）：复位设备后重连再试；反复出现见 KI-005',
  busy: '当前状态下不能这么做，先停止正在进行的操作',
  internal: '查看诊断日志；这属于未预期错误',
}

/**
 * 事件名 → 中文说明。只用于展示，键名/事件名本身以 config/README.md 为准。
 *
 * v3 起只有 3 个事件：一个键一个动作。按下触发，抬起/短按/双击/长按都不参与映射。
 * OK 是开关式（按一下开、再按一下关），所以不再像 v2 那样分"开/关"两行。
 */
export const EVENT_LABEL: Record<string, string> = {
  up: '上键',
  down: '下键',
  ok: 'OK（PTT 开关）',
  // 下面两行不是按键，是"采集状态钩子"：开麦/关麦的那一刻注入的快捷键（点按）。
  'capture.start': '开麦时 · 注入',
  'capture.stop': '关麦时 · 注入',
}

/** 常用键名，给输入框做候选；最终合法性由后端校验。 */
export const KEY_CHOICES = [
  'enter',
  'esc',
  'space',
  'tab',
  'backspace',
  'delete',
  'up',
  'down',
  'left',
  'right',
  'home',
  'end',
  'pageup',
  'pagedown',
  'insert',
  'printscreen',
  'menu',
  'capslock',
  // 修饰键本身作为目标键（macOS 上 Alt = Option，左右键码 58/61 不同）。
  'lalt',
  'ralt',
  'f1',
  'f2',
  'f3',
  'f4',
  'f5',
  'f6',
  'f7',
  'f8',
  'f9',
  'f10',
  'f11',
  'f12',
  'a',
  'b',
  'c',
  'd',
  'e',
  'f',
  'g',
  'h',
  'i',
  'j',
  'k',
  'l',
  'm',
  'n',
  'o',
  'p',
  'q',
  'r',
  's',
  't',
  'u',
  'v',
  'w',
  'x',
  'y',
  'z',
  'ptt-start',
  'ptt-stop',
]
