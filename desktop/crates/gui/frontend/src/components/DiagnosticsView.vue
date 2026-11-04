<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import type { LogEntry, LogLevel } from '../lib/types'

const props = defineProps<{
  entries: LogEntry[]
  logPath: string
}>()

const emit = defineEmits<{
  (e: 'copy'): void
  (e: 'reveal'): void
}>()

const level = ref<'all' | LogLevel>('all')
const query = ref('')
const follow = ref(true)
const rowsEl = ref<HTMLElement | null>(null)

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
      <input v-model="query" type="search" placeholder="过滤消息 / 来源" />
      <label class="follow">
        <input v-model="follow" type="checkbox" />
        跟随最新
      </label>
      <span class="count">{{ visible.length }} / {{ entries.length }}</span>
      <span class="spacer" />
      <button @click="emit('copy')">复制诊断包</button>
      <button @click="emit('reveal')">打开日志目录</button>
    </div>

    <p class="path mono" :title="logPath">{{ logPath }}</p>

    <div ref="rowsEl" class="rows scroll">
      <p v-if="visible.length === 0" class="empty">没有匹配的日志。</p>
      <div v-for="(e, i) in visible" :key="`${e.ts_ms}-${i}`" class="log" :data-level="e.level">
        <span class="time mono">{{ fmtTime(e.ts_ms) }}</span>
        <span class="lvl">{{ e.level }}</span>
        <span class="src">{{ e.source }}</span>
        <span class="msg">{{ e.message }}</span>
      </div>
    </div>
  </div>
</template>

<style scoped>
.diag {
  display: flex;
  flex-direction: column;
  min-height: 0;
  height: 100%;
  gap: 8px;
}

.toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.toolbar .spacer {
  flex: 1;
}

select,
input[type='search'] {
  font: inherit;
  font-size: 12px;
  color: var(--text);
  background: var(--panel-2);
  border: 1px solid var(--line);
  border-radius: 7px;
  padding: 4px 8px;
}

input[type='search'] {
  width: 190px;
}

input[type='search']:focus,
select:focus {
  outline: none;
  border-color: #4c8dff88;
}

.follow {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 12px;
  color: var(--muted);
}

.count {
  font-size: 11.5px;
  color: var(--dim);
  font-variant-numeric: tabular-nums;
}

.path {
  margin: 0;
  color: var(--dim);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.rows {
  flex: 1;
  min-height: 0;
  background: var(--panel);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius);
  padding: 6px;
}

.empty {
  color: var(--dim);
  padding: 16px;
  margin: 0;
}

.log {
  display: grid;
  grid-template-columns: 92px 46px 58px minmax(0, 1fr);
  gap: 8px;
  padding: 3px 8px;
  border-radius: 6px;
  font-size: 12px;
  line-height: 1.55;
  user-select: text;
}

.log:hover {
  background: #ffffff06;
}

.time {
  color: var(--dim);
  font-variant-numeric: tabular-nums;
}

.lvl {
  text-transform: uppercase;
  font-size: 10px;
  font-weight: 600;
  letter-spacing: 0.04em;
  padding-top: 2px;
  color: var(--muted);
}

.src {
  color: var(--dim);
  font-size: 11px;
}

.msg {
  color: var(--text);
  overflow-wrap: anywhere;
}

.log[data-level='debug'] .msg {
  color: var(--muted);
}

.log[data-level='warn'] .lvl,
.log[data-level='warn'] .msg {
  color: var(--warn);
}

.log[data-level='error'] .lvl,
.log[data-level='error'] .msg {
  color: var(--err);
}
</style>
