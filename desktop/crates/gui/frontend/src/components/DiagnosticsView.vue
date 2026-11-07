<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import type { LogEntry, LogLevel } from '../lib/types'

const props = defineProps<{
  entries: LogEntry[]
  logPath: string
  /** 版本号放在这里而不是顶栏：诊断页是"我在报问题"的地方，版本是证据的一部分。 */
  version: string
}>()

const emit = defineEmits<{
  (e: 'copy'): void
  (e: 'reveal'): void
}>()

const level = ref<'all' | LogLevel>('all')
const query = ref('')
const follow = ref(true)
const rowsEl = ref<HTMLElement | null>(null)

/** 当前滚动缓冲里还有多少条 error。注意它**不会**因进页面而清零——
 *  清零的是 tab 上的"未读角标"（App.vue 的 errorCount）。两者语义不同，别混。 */
const errorTotal = computed(() => props.entries.filter((e) => e.level === 'error').length)

// 「跟随最新」：新条目到达时把列表滚到底。只在用户勾选时做，
// 否则用户往上翻查旧日志会被不断拽回底部。
watch(
  () => props.entries.length,
  async () => {
    if (!follow.value) return
    await nextTick()
    const el = rowsEl.value
    if (el) el.scrollTop = el.scrollHeight
  },
)

const LEVEL_WEIGHT: Record<LogLevel, number> = { debug: 0, info: 1, warn: 2, error: 3 }

const visible = computed(() => {
  const q = query.value.trim().toLowerCase()
  const min = level.value === 'all' ? -1 : LEVEL_WEIGHT[level.value]
  return props.entries.filter((e) => {
    if (LEVEL_WEIGHT[e.level] < min) return false
    if (!q) return true
    return (
      e.message.toLowerCase().includes(q) ||
      e.source.toLowerCase().includes(q) ||
      e.level.includes(q)
    )
  })
})

function fmtTime(ms: number): string {
  const d = new Date(ms)
  const hh = String(d.getHours()).padStart(2, '0')
  const mm = String(d.getMinutes()).padStart(2, '0')
  const ss = String(d.getSeconds()).padStart(2, '0')
  const mss = String(d.getMilliseconds()).padStart(3, '0')
  return `${hh}:${mm}:${ss}.${mss}`
}
</script>

<template>
  <div class="diag">
    <div class="toolbar">
      <select v-model="level" aria-label="日志等级">
        <option value="all">全部等级</option>
        <option value="info">info 及以上</option>
        <option value="warn">warn 及以上</option>
        <option value="error">只看 error</option>
      </select>
      <input v-model="query" type="search" class="search" placeholder="过滤消息 / 来源" />
      <label class="check">
        <input v-model="follow" type="checkbox" />
        跟随最新
      </label>
      <span class="count mono">{{ visible.length }} / {{ entries.length }}</span>
      <span class="spacer" />
      <button class="sm" @click="emit('copy')">复制诊断包</button>
      <button class="sm ghost" @click="emit('reveal')">打开日志目录</button>
    </div>

    <div ref="rowsEl" class="rows scroll">
      <p v-if="visible.length === 0" class="empty">没有匹配的日志。</p>
      <div v-for="(e, i) in visible" :key="`${e.ts_ms}-${i}`" class="log" :data-level="e.level">
        <span class="time">{{ fmtTime(e.ts_ms) }}</span>
        <span class="lvl">{{ e.level }}</span>
        <span class="src">{{ e.source }}</span>
        <span class="msg">{{ e.message }}</span>
      </div>
    </div>

    <div class="foot">
      <span class="mono">v{{ version }}</span>
      <span class="sep">·</span>
      <span class="mono path" :title="logPath">{{ logPath }}</span>
      <span class="spacer" />
      <span class="dim">缓冲内 error {{ errorTotal }} 条 · tab 角标进本页即清零</span>
    </div>
  </div>
</template>

<style scoped>
.diag {
  display: flex;
  flex-direction: column;
  min-height: 0;
  flex: 1 1 auto;
  gap: var(--s2);
}

.toolbar {
  display: flex;
  align-items: center;
  gap: var(--s2);
  flex-wrap: wrap;
}

.search {
  width: 176px;
}

.search::placeholder {
  color: var(--dim);
}

.check {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: var(--fs-11);
  color: var(--muted);
  white-space: nowrap;
  cursor: pointer;
}

.count {
  font-size: var(--fs-11);
  color: var(--dim);
  font-variant-numeric: tabular-nums;
}

.rows {
  flex: 1 1 auto;
  min-height: 0;
  background: var(--panel);
  border: 1px solid var(--line-soft);
  border-radius: var(--r-md);
  padding: 6px;
  overflow-y: auto;
}

.empty {
  color: var(--dim);
  font-size: var(--fs-11);
  padding: 16px;
  margin: 0;
}

.log {
  display: grid;
  grid-template-columns: 88px 40px 52px minmax(0, 1fr);
  gap: var(--s2);
  padding: 3px 8px;
  border-radius: var(--r-sm);
  font-size: var(--fs-11);
  line-height: 1.6;
  font-family: var(--mono);
}

.log:hover {
  background: #ffffff05;
}

.time {
  color: var(--dim);
  font-variant-numeric: tabular-nums;
}

.lvl {
  font-size: var(--fs-10);
  font-weight: 700;
  letter-spacing: 0.04em;
  color: var(--dim);
}

.src {
  color: var(--dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.msg {
  color: var(--muted);
  overflow-wrap: anywhere;
  font-family: -apple-system, 'PingFang SC', sans-serif;
  user-select: text;
}

.log[data-level='debug'] .msg {
  color: var(--dim);
}

.log[data-level='warn'] .lvl,
.log[data-level='warn'] .msg {
  color: var(--warn);
}

.log[data-level='error'] .lvl,
.log[data-level='error'] .msg {
  color: var(--err);
}

.foot {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: var(--fs-10);
  color: var(--dim);
  min-width: 0;
}

.foot .path {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
}

.sep {
  color: var(--line);
}

.dim {
  color: var(--dim);
  white-space: nowrap;
}
</style>
