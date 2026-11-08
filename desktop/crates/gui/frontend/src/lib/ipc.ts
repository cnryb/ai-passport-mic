// 唯一的 IPC 入口。组件不允许直接 invoke 字符串命令名——
// 命令改名时只改这里，漏改会编译报错。

import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type {
  AppEvent,
  AppError,
  AppSnapshot,
  BindingInput,
  ErrorCode,
  FirmwareStatus,
  FlashReport,
  KeymapView,
  ProbeResult,
  QualityReport,
  SessionSummary,
  SinkCheckReport,
  SinkView,
} from './types'

export const api = {
  snapshot: () => invoke<AppSnapshot>('snapshot'),
  detectAdapter: () => invoke<void>('detect_adapter'),
  // Tauri 2 会把 JS 的 camelCase 参数映射到 Rust 的 snake_case 形参。
  scanStart: (windowMs?: number) => invoke<void>('scan_start', { windowMs }),
  scanStop: () => invoke<void>('scan_stop'),
  connect: (deviceId: string) => invoke<void>('connect', { deviceId }),
  setAutoConnect: (enabled: boolean) => invoke<void>('set_auto_connect', { enabled }),
  disconnect: () => invoke<void>('disconnect'),

  listSinks: () => invoke<SinkView[]>('list_sinks'),
  setSink: (name: string) => invoke<void>('set_sink', { name }),
  /** 一键验证虚拟声卡：写一段已知电平的正弦，再从输入端回读。阻塞约 `seconds` 秒。 */
  checkSink: (name: string, seconds?: number) =>
    invoke<SinkCheckReport>('check_sink', { name, seconds }),
  openSoundSettings: () => invoke<void>('open_sound_settings'),
  openVirtualAudioDownload: () => invoke<void>('open_virtual_audio_download'),
  setBufferTarget: (ms: number) => invoke<void>('set_buffer_target', { ms }),
  setAutoStopMin: (minutes: number) => invoke<void>('set_auto_stop_min', { minutes }),
  setSaveWav: (save: boolean) => invoke<void>('set_save_wav', { save }),
  startStream: (saveWav: boolean) => invoke<string>('start_stream', { saveWav }),
  stopStream: () => invoke<SessionSummary>('stop_stream'),

  analyzeSession: (sessionId: string) => invoke<QualityReport>('analyze_session', { sessionId }),
  analyzeSessionJson: (sessionId: string) =>
    invoke<string>('analyze_session_json', { sessionId }),
  revealPath: (path: string) => invoke<void>('reveal_path', { path }),

  keymapView: () => invoke<KeymapView>('keymap_view'),
  keymapSave: (bindings: BindingInput[]) => invoke<KeymapView>('keymap_save', { bindings }),
  keymapImport: (text: string) => invoke<KeymapView>('keymap_import', { text }),
  keymapReset: () => invoke<KeymapView>('keymap_reset'),
  keymapText: () => invoke<string>('keymap_text'),

  injectPermission: () => invoke<boolean>('inject_permission'),
  openAccessibilitySettings: () => invoke<void>('open_accessibility_settings'),

  firmwareStatus: () => invoke<FirmwareStatus>('firmware_status'),
  firmwareProbe: (port: string) => invoke<ProbeResult>('firmware_probe', { port }),
  firmwareFlash: (port: string) => invoke<FlashReport>('firmware_flash', { port }),

  diagnosticsText: () => invoke<string>('diagnostics_text'),
  /**
   * 上报一段实测帧率（5 s 窗口），写进诊断日志供 `test-gui-perf.sh` 汇总。
   *
   * 为什么走日志而不是返回值：帧率是**持续**指标，需要整场均值/最小值，
   * 单次调用没有意义。日志是唯一天然的时序通道。
   */
  reportFps: (fps: number, windowMs: number) =>
    invoke<void>('report_fps', { fps, windowMs }),  logPath: () => invoke<string>('log_path'),
  revealLog: () => invoke<void>('reveal_log'),
}

export function onAppEvent(handler: (event: AppEvent) => void): Promise<UnlistenFn> {
  return listen<AppEvent>('app-event', (e) => handler(e.payload))
}

/** 烧写进度（0–100）。按字节数折算，三段不是各占 33%。 */
export function onFirmwareProgress(handler: (percent: number) => void): Promise<UnlistenFn> {
  return listen<number>('firmware-progress', (e) => handler(e.payload))
}

const KNOWN_CODES: ErrorCode[] = [
  'bt_unauthorized',
  'bt_unavailable',
  'device_not_found',
  'connect_timeout',
  'subscribe_failed',
  'sink_missing',
  'sink_rate_unsupported',
  'protocol_mismatch',
  'keymap_invalid',
  'flash_no_device',
  'firmware_image_invalid',
  'flash_failed',
  'flash_verify_failed',
  'capture_not_starting',
  'busy',
  'internal',
]

/**
 * 命令失败时 Tauri 抛出的是被序列化的 `AppError`，但网络/参数错误会是普通字符串。
 * 这里统一成 `AppError`，否则界面上会出现 `[object Object]`。
 */
export function asAppError(err: unknown): AppError {
  if (typeof err === 'object' && err !== null && 'code' in err) {
    const candidate = err as { code?: unknown; detail?: unknown }
    const code = KNOWN_CODES.find((c) => c === candidate.code)
    if (code) {
      return { code, detail: String(candidate.detail ?? '') }
    }
  }
  return { code: 'internal', detail: typeof err === 'string' ? err : JSON.stringify(err) }
}
