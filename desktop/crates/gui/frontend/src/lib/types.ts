// 与 Rust 侧 IPC 契约一一对应（docs/GUI.md §6）。改这里必须同步改
// desktop/crates/app/src/state.rs 与 events.rs。

export type ErrorCode =
  | 'bt_unauthorized'
  | 'bt_unavailable'
  | 'device_not_found'
  | 'connect_timeout'
  | 'subscribe_failed'
  | 'sink_missing'
  | 'sink_rate_unsupported'
  | 'protocol_mismatch'
  | 'busy'
  | 'internal'

export interface AppError {
  code: ErrorCode
  detail: string
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

export interface AppSnapshot {
  version: string
  state: AppState
  devices: DeviceView[]
  log_tail: LogEntry[]
  log_path: string
  log_dir: string
}

export type AppEvent =
  | { type: 'state-changed'; state: AppState }
  | { type: 'devices'; devices: DeviceView[] }
  | { type: 'event-log'; entry: LogEntry }

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
  busy: '当前状态下不能这么做，先停止正在进行的操作',
  internal: '查看诊断日志；这属于未预期错误',
}
