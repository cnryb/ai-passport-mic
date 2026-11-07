<script setup lang="ts">
/**
 * 按键映射（G5）：3 个按键 + 2 个采集钩子的编辑、导入导出、恢复默认。
 *
 * v3 起一个键只有一个动作，按下即触发；事件表由后端给（`apm_protocol::keymap::EVENTS`），
 * 前端只按顺序渲染，不自己拼事件名。
 *
 * 校验在 Rust 侧（`apm_app::keymap_store` → `apm_protocol::keymap`），
 * 前端只收集输入，不猜"这个键名算不算合法"。失败时整份拒绝，不做部分生效。
 *
 * 注入（M3）已实现：开麦/关麦钩子真的会往前台应用敲键。前提是**辅助功能权限**，
 * 没权限时系统静默丢弃按键——所以页面顶部有权限横幅：缺权限时分步引导 + 一键
 * 跳系统设置，授权后自动消失（缺权限状态下每 2 秒复查一次）。
 */
import { onMounted, onUnmounted, ref, watch } from 'vue'
import { EVENT_LABEL, KEY_CHOICES, type BindingInput, type KeymapView } from '../lib/types'
import { api } from '../lib/ipc'

const props = defineProps<{
  view: KeymapView | null
  busy: boolean
  error: string | null
}>()

const emit = defineEmits<{
  (e: 'save', bindings: BindingInput[]): void
  (e: 'reset'): void
  (e: 'import', text: string): void
  (e: 'copy'): void
}>()

const MODS = ['ctrl', 'shift', 'alt', 'win'] as const

const rows = ref<BindingInput[]>([])
const importText = ref('')
const showImport = ref(false)

// 辅助功能权限：null = 还没查到（避免闪横幅）；false = 缺权限，显示引导；
// true = 已就绪，横幅收起。缺权限时轮询复查，用户勾完回来不用手动刷新。
const injectOk = ref<boolean | null>(null)
let pollTimer: ReturnType<typeof setInterval> | null = null

async function checkInject() {
  try {
    injectOk.value = await api.injectPermission()
  } catch {
    injectOk.value = null
  }
  if (injectOk.value === false) startPolling()
  else stopPolling()
}

function startPolling() {
  if (pollTimer) return
  pollTimer = setInterval(checkInject, 2_000)
}

function stopPolling() {
  if (pollTimer) {
    clearInterval(pollTimer)
    pollTimer = null
  }
}

async function openAccessibilitySettings() {
  try {
    await api.openAccessibilitySettings()
  } catch {
    // 打不开就退回手动路径，横幅里的文字本身就是指引
  }
}

onMounted(checkInject)
onUnmounted(stopPolling)

watch(
  () => props.view,
  (v) => {
    if (!v) return
    rows.value = v.bindings.map((b) => ({
      event: b.event,
      modifiers: [...b.modifiers],
      key: b.key,
      repeat: b.repeat,
    }))
  },
  { immediate: true },
)

function locked(event: string): boolean {
  return props.view?.bindings.find((b) => b.event === event)?.locked ?? false
}

function toggleMod(row: BindingInput, mod: string) {
  if (locked(row.event)) return
  const at = row.modifiers.indexOf(mod)
  if (at >= 0) row.modifiers.splice(at, 1)
  else row.modifiers.push(mod)
}

function save() {
  emit(
    'save',
    rows.value.map((r) => ({
      event: r.event,
      modifiers: [...r.modifiers],
      key: r.key.trim().toLowerCase(),
      repeat: r.repeat,
    })),
  )
}

function doImport() {
  emit('import', importText.value)
  showImport.value = false
}
</script>

<template>
  <div class="keymap">
    <div class="toolbar">
      <span class="dim path mono" :title="view?.path">{{ view?.path ?? '…' }}</span>
      <span class="spacer" />
      <button @click="showImport = !showImport">{{ showImport ? '收起导入' : '导入 JSON' }}</button>
      <button :disabled="busy" @click="emit('copy')">导出</button>
      <button :disabled="busy" @click="emit('reset')">恢复默认</button>
      <button class="primary" :disabled="busy" @click="save">保存</button>
    </div>

    <div v-if="injectOk === false" class="perm" role="alert">
      <p class="perm-title"><strong>注入还没生效：缺少辅助功能权限</strong></p>
      <ol class="perm-steps">
        <li>点下面按钮——系统会弹「AI Passport Mic 想要控制你的电脑」，点<strong>打开系统设置</strong></li>
        <li>辅助功能列表里会出现 <strong>AI Passport Mic</strong>（列表里原本没有它是正常的，点过按钮才会登记），打开它的开关</li>
        <li>回到本页——横幅会自动消失，不需要重启 App</li>
      </ol>
      <p class="perm-warn dim">
        没有这个权限时，系统会<strong>静默丢弃</strong>注入的按键：开麦/关麦看起来正常，
        但目标应用什么都没收到，也不会有任何报错。
      </p>
      <div class="perm-actions">
        <button class="primary" @click="openAccessibilitySettings">打开系统设置</button>
        <button @click="checkInject">重新检查</button>
      </div>
    </div>
    <p v-else-if="injectOk === true" class="ok-line">辅助功能权限已就绪，开麦 / 关麦的快捷键注入可用。</p>

    <p class="note">
      上三行是按键（按下即触发）；下两行是<strong>采集钩子</strong>：开麦 / 关麦的那一刻，
      自动向当前应用注入一个快捷键（点按一次）。键名 <code>ralt</code> / <code>lalt</code>
      是单独点按右 / 左 Alt（macOS 上即右 / 左 Option），适合"单按右 Alt"这类热键。
      OK 是<strong>开关式</strong>：按一下开、再按一下关，必须是 <code>ptt-toggle</code>（本页已锁定该项）。
      开关式没有"松手自动停"——忘记关麦是真实风险，注入层配套的超时兜底见 ROADMAP M3。
    </p>

    <p v-if="error" class="err">{{ error }}</p>
    <p v-if="view?.error" class="err">{{ view.error }}</p>

    <div v-if="showImport" class="import">
      <textarea v-model="importText" rows="6" placeholder="粘贴 keymap JSON（schema: ai-passport-mic-keymap-4；v3 也能导入，会补上空 hooks 升到 v4）" />
      <button :disabled="!importText.trim()" @click="doImport">导入并校验</button>
    </div>

    <div class="rows scroll">
      <div v-for="row in rows" :key="row.event" class="row" :data-locked="locked(row.event)">
        <span class="name">{{ EVENT_LABEL[row.event] ?? row.event }}</span>

        <div class="mods">
          <button
            v-for="m in MODS"
            :key="m"
            class="chip"
            :class="{ on: row.modifiers.includes(m) }"
            :disabled="locked(row.event)"
            @click="toggleMod(row, m)"
          >
            {{ m }}
          </button>
        </div>

        <input
          v-model="row.key"
          class="key"
          list="key-choices"
          :disabled="locked(row.event)"
          placeholder="不动作"
          spellcheck="false"
        />

        <select v-model.number="row.repeat" :disabled="locked(row.event)" aria-label="连发次数">
          <option :value="1">×1</option>
          <option :value="2">×2</option>
          <option :value="3">×3</option>
          <option :value="4">×4</option>
        </select>
      </div>
    </div>

    <datalist id="key-choices">
      <option v-for="k in KEY_CHOICES" :key="k" :value="k" />
    </datalist>
  </div>
</template>

<style scoped>
.keymap {
  display: flex;
  flex-direction: column;
  gap: 8px;
  min-height: 0;
  height: 100%;
}

.toolbar {
  display: flex;
  align-items: center;
  gap: 6px;
}

.spacer {
  flex: 1;
}

.path {
  font-size: 11px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 46%;
}

.note {
  margin: 0;
  font-size: 11.5px;
  line-height: 1.65;
  color: var(--muted);
}

.perm {
  border: 1px solid #e6a23c55;
  background: #e6a23c12;
  border-radius: var(--radius);
  padding: 10px 12px;
  font-size: 12px;
  line-height: 1.6;
}

.perm-title {
  margin: 0 0 4px;
  color: #f2c28a;
}

.perm-steps {
  margin: 0 0 6px;
  padding-left: 20px;
}

.perm-warn {
  margin: 0 0 8px;
  font-size: 11px;
}

.perm-actions {
  display: flex;
  gap: 8px;
}

.ok-line {
  margin: 0;
  font-size: 11.5px;
  color: #7ecf8e;
}

.err {
  margin: 0;
  color: var(--err);
  font-size: 12px;
  user-select: text;
}

.import {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.import textarea {
  font: 11.5px/1.6 var(--mono);
  color: var(--text);
  background: var(--panel-2);
  border: 1px solid var(--line);
  border-radius: 8px;
  padding: 8px;
  resize: vertical;
}

.import button {
  align-self: flex-start;
}

.rows {
  flex: 1;
  min-height: 0;
  border: 1px solid var(--line-soft);
  border-radius: var(--radius);
  background: var(--panel);
  padding: 4px;
}

.row {
  display: grid;
  grid-template-columns: 150px 176px minmax(0, 1fr) 64px;
  align-items: center;
  gap: 8px;
  padding: 4px 8px;
  border-radius: 7px;
  font-size: 12px;
}

.row:hover {
  background: #ffffff05;
}

.row[data-locked='true'] {
  background: #4c8dff0a;
}

.name {
  color: var(--text);
}

.mods {
  display: flex;
  gap: 4px;
}

.chip {
  padding: 1px 7px;
  font-size: 10.5px;
  border-radius: 999px;
  background: transparent;
  color: var(--dim);
  border: 1px solid var(--line);
  min-width: 0;
}

.chip.on {
  background: #4c8dff22;
  border-color: #4c8dff66;
  color: #b9d3ff;
}

.key,
select {
  font: 12px/1.4 var(--mono);
  color: var(--text);
  background: var(--panel-2);
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 3px 7px;
  width: 100%;
}

.key:focus,
select:focus {
  outline: none;
  border-color: #4c8dff88;
}

.key:disabled,
select:disabled {
  opacity: 0.6;
}
</style>
