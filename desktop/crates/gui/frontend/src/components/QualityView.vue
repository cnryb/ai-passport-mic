<script setup lang="ts">
/**
 * 音质报告（G4）。字段与 `apmctl analyze --json` 同源（`apm_app::report`），
 * 这里只负责展示，不重新计算任何指标。
 *
 * 一句话口径必须一直在：软件分析通过 ≠ 真机链路/会议软件通过。
 */
import type { QualityReport } from '../lib/types'

defineProps<{
  report: QualityReport | null
  sessionId: string | null
  wavPath: string | null
  loading: boolean
  error: string | null
}>()

const emit = defineEmits<{
  (e: 'analyze'): void
  (e: 'copy-json'): void
  (e: 'reveal'): void
}>()

function fmt(v: number | null, digits = 2, unit = ''): string {
  return v === null ? '—' : `${v.toFixed(digits)}${unit}`
}
</script>

<template>
  <div class="quality">
    <div class="toolbar">
      <span v-if="sessionId" class="meta mono">会话 {{ sessionId }}</span>
      <span v-else class="meta">还没有会话</span>
      <span class="spacer" />
      <button :disabled="!wavPath || loading" @click="emit('analyze')">
        {{ loading ? '分析中…' : '重新分析' }}
      </button>
      <button :disabled="!report" @click="emit('copy-json')">复制 JSON</button>
      <button :disabled="!wavPath" @click="emit('reveal')">在访达中显示</button>
    </div>

    <p v-if="wavPath" class="path mono">{{ wavPath }}</p>
    <p v-else class="path dim">
      {{ sessionId ? '这次会话没有录音（开始推流时未勾选「同时保存 WAV」），无法出报告。' : '连上设备、开始一次推流后，这里会出现报告。' }}
    </p>

    <p v-if="error" class="err">{{ error }}</p>

    <template v-if="report">
      <div class="verdict" :data-pass="report.pass">
        <strong>{{ report.pass ? '软件分析通过' : '软件分析不通过' }}</strong>
        <span class="mono">{{ report.verdict }}</span>
        <p class="dim">
          这只说明文件本身没有明显削波 / 直流偏置 / 电平异常。真机链路、虚拟声卡与会议软件
          端到端是否通过，必须另外测；听感也没有自动化结论。
        </p>
      </div>

      <div class="grid">
        <div class="cell"><span class="dim">时长</span><b>{{ fmt(report.duration_s, 2, ' s') }}</b></div>
        <div class="cell"><span class="dim">RMS</span><b>{{ fmt(report.rms_dbfs, 2, ' dBFS') }}</b></div>
        <div class="cell"><span class="dim">峰值</span><b>{{ fmt(report.peak_dbfs, 2, ' dBFS') }}</b></div>
        <div class="cell"><span class="dim">削波</span><b>{{ fmt(report.clipping_pct, 3, ' %') }}</b></div>
        <div class="cell"><span class="dim">直流偏置</span><b>{{ fmt(report.dc_offset_pct, 3, ' %') }}</b></div>
        <div class="cell"><span class="dim">过零率</span><b>{{ fmt(report.zero_crossing_pct, 2, ' %') }}</b></div>
        <div class="cell"><span class="dim">毛刺</span><b>{{ report.glitch_events }} 段 / {{ fmt(report.glitch_ms, 0, ' ms') }}</b></div>
        <div class="cell"><span class="dim">格式</span><b>{{ report.sample_rate }} Hz · {{ report.channels }} ch</b></div>
      </div>

      <table class="checks">
        <thead>
          <tr><th>指标</th><th>实测</th><th>判定</th><th>阈值</th></tr>
        </thead>
        <tbody>
          <tr v-for="c in report.checks" :key="c.name" :data-ok="c.ok">
            <td>{{ c.name }}</td>
            <td class="mono">{{ c.actual }}</td>
            <td class="mark">{{ c.ok ? 'OK' : 'FAIL' }}</td>
            <td class="dim">{{ c.expected }}</td>
          </tr>
        </tbody>
      </table>

      <p class="dim foot">
        链路丢块、启动延迟与稳态端到端延迟不在 WAV 里，报告里写 null 而不是 0 —— 未测就是未测。
      </p>
    </template>
  </div>
</template>

<style scoped>
.quality {
  display: flex;
  flex-direction: column;
  gap: 8px;
  min-height: 0;
  height: 100%;
  overflow: auto;
  scrollbar-width: thin;
}

.toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
}

.spacer {
  flex: 1;
}

.meta {
  color: var(--muted);
  font-size: 11.5px;
}

.path {
  margin: 0;
  color: var(--dim);
  font-size: 11.5px;
  overflow-wrap: anywhere;
  user-select: text;
}

.err {
  margin: 0;
  color: var(--err);
  font-size: 12px;
}

.verdict {
  border: 1px solid var(--line);
  border-left: 3px solid var(--warn);
  background: var(--panel);
  border-radius: 8px;
  padding: 9px 12px;
  display: flex;
  flex-direction: column;
  gap: 3px;
}

.verdict[data-pass='true'] {
  border-left-color: var(--ok);
}

.verdict strong {
  font-size: 12.5px;
}

.verdict .mono {
  color: var(--dim);
  font-size: 11px;
}

.verdict p {
  margin: 2px 0 0;
  font-size: 11.5px;
  line-height: 1.65;
}

.grid {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 6px;
}

.cell {
  background: var(--panel);
  border: 1px solid var(--line-soft);
  border-radius: 8px;
  padding: 7px 9px;
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.cell span {
  font-size: 10.5px;
}

.cell b {
  font-size: 12.5px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.checks {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.checks th {
  text-align: left;
  color: var(--dim);
  font-weight: 500;
  font-size: 10.5px;
  letter-spacing: 0.04em;
  text-transform: uppercase;
  padding: 4px 8px;
  border-bottom: 1px solid var(--line-soft);
}

.checks td {
  padding: 5px 8px;
  border-bottom: 1px solid #1a212b;
}

.checks .mark {
  font-family: var(--mono);
  font-size: 11px;
  color: var(--ok);
}

.checks tr[data-ok='false'] .mark {
  color: var(--err);
}

.foot {
  font-size: 11px;
  line-height: 1.6;
  margin: 0 0 4px;
}
</style>
