<script setup lang="ts">
/**
 * 音质报告（G4）。字段与 `apmctl analyze --json` 同源（`apm_app::report`），
 * 这里只负责展示，不重新计算任何指标。
 *
 * 一句话口径必须一直在：软件分析通过 ≠ 真机链路/会议软件通过。
 *
 * ## 为什么只有一张表（2026-11-07 重做）
 * 旧版把同一组数字讲了三遍：verdict 文本里一遍（`rms=-22.10 … clipping=0.000%`）、
 * 8 格指标卡一遍、判定表一遍。三处口径相同但格式不同，读的人要自己判断"哪个算数"。
 * 现在合成一张表：指标 / 实测 / 判定 / 阈值同列，verdict 只留结论句与两个概览事实。
 * 表格本身就是 `report.checks` 的渲染，**没有新增计算**。
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

function fmtDuration(s: number): string {
  return `${s.toFixed(2)} s`
}

/** 判定表里没有、但概览行需要的事实（都在 report 里，不重算）。 */
function formatFacts(report: QualityReport): string {
  const ch = `${report.sample_rate} Hz · ${report.channels} ch${report.downmixed ? ' · 已下混' : ''}`
  return ch
}
</script>

<template>
  <div class="quality">
    <div class="toolbar">
      <div class="session">
        <b>{{ sessionId ?? '还没有会话' }}</b>
        <span v-if="wavPath" class="mono" :title="wavPath">{{ wavPath }}</span>
        <span v-else class="dim">
          {{ sessionId ? '这次会话没有录音（开始推流时未勾选「保存 WAV」），无法出报告' : '连上设备、开始一次推流后，这里会出现报告' }}
        </span>
      </div>
      <span class="spacer" />
      <button class="sm" :disabled="!wavPath || loading" @click="emit('analyze')">
        {{ loading ? '分析中…' : '重新分析' }}
      </button>
      <button class="sm" :disabled="!report" @click="emit('copy-json')">复制 JSON</button>
      <button class="sm ghost" :disabled="!wavPath" @click="emit('reveal')">在访达中显示</button>
    </div>

    <p v-if="error" class="err">{{ error }}</p>

    <template v-if="report">
      <div class="verdict" :data-pass="report.pass">
        <div class="v-main">
          <b>{{ report.pass ? '软件分析通过' : '软件分析不通过' }}</b>
          <span>文件本身{{ report.pass ? '无明显' : '存在' }}削波 / 直流偏置 / 电平异常</span>
        </div>
        <div class="v-facts mono">
          <span>{{ fmtDuration(report.duration_s) }}</span>
          <span>{{ formatFacts(report) }}</span>
        </div>
      </div>

      <p v-if="report.rate_note" class="warn-note">{{ report.rate_note }}</p>

      <table class="checks">
        <colgroup>
          <col style="width: 30%" />
          <col style="width: 26%" />
          <col style="width: 14%" />
          <col style="width: 30%" />
        </colgroup>
        <thead>
          <tr><th>指标</th><th>实测</th><th>判定</th><th>阈值</th></tr>
        </thead>
        <tbody>
          <tr v-for="c in report.checks" :key="c.name" :data-ok="c.ok">
            <td>{{ c.name }}</td>
            <td class="num">{{ c.actual }}</td>
            <td class="mark">{{ c.ok ? 'OK' : 'FAIL' }}</td>
            <td class="th">{{ c.expected }}</td>
          </tr>
        </tbody>
      </table>

      <p class="note">
        这只说明文件本身合格。<b>链路丢块、启动延迟与稳态端到端延迟不在 WAV 里</b>，
        报告里写 null 而不是 0 —— 未测就是未测。真机链路、虚拟声卡与会议软件的端到端结论必须另外测；
        听感也没有自动化结论。
        <template v-if="report.verdict">
          <br /><span class="dim mono">{{ report.verdict }}</span>
        </template>
      </p>
    </template>
  </div>
</template>

<style scoped>
.quality {
  display: flex;
  flex-direction: column;
  gap: var(--gap-card);
  min-height: 0;
}

.toolbar {
  display: flex;
  align-items: center;
  gap: var(--s2);
  flex-wrap: wrap;
}

.session {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
  flex: 1 1 auto;
}

.session b {
  font-family: var(--mono);
  font-size: var(--fs-12);
  font-weight: 500;
}

.session span {
  font-size: var(--fs-10);
  color: var(--dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  user-select: text;
}

.err {
  margin: 0;
  color: var(--err);
  font-size: var(--fs-12);
  user-select: text;
}

.warn-note {
  margin: 0;
  color: var(--warn);
  font-size: var(--fs-11);
  line-height: 1.6;
}

.verdict {
  display: flex;
  align-items: center;
  gap: 10px;
  border: 1px solid var(--line-soft);
  border-left: 3px solid var(--warn);
  background: var(--panel);
  border-radius: var(--r-sm);
  padding: 9px 12px;
}

.verdict[data-pass='true'] {
  border-left-color: var(--ok);
}

.v-main {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}

.v-main b {
  font-size: var(--fs-13);
  font-weight: 600;
}

.v-main span {
  font-size: var(--fs-11);
  color: var(--dim);
}

.v-facts {
  margin-left: auto;
  display: flex;
  gap: 14px;
  font-size: var(--fs-11);
  color: var(--muted);
  white-space: nowrap;
}

.checks {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-12);
}

.checks th {
  text-align: left;
  color: var(--dim);
  font-weight: 500;
  font-size: var(--fs-10);
  letter-spacing: 0.05em;
  text-transform: uppercase;
  padding: 5px 8px;
  border-bottom: 1px solid var(--line);
}

.checks td {
  padding: 6px 8px;
  border-bottom: 1px solid var(--line-soft);
}

.checks td.num {
  font-family: var(--mono);
  font-variant-numeric: tabular-nums;
}

.checks td.mark {
  font-family: var(--mono);
  font-size: var(--fs-11);
  font-weight: 600;
  color: var(--ok);
}

.checks tr[data-ok='false'] td.mark {
  color: var(--err);
}

.checks td.th {
  color: var(--dim);
}

.dim {
  color: var(--dim);
}
</style>
