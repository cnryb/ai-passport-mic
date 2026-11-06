<script setup lang="ts">
/**
 * 开箱页（M6）：识别设备 → 比对版本 → 写入固件 → 校验。
 *
 * 口径见 `docs/PRODUCT.md` 的「开箱体验」，实现见 `apm_app::firmware`：
 * - 烧写会让设备**复位**进下载模式，正在推流时别点；
 * - 不擦 NVS：配对与按键映射保留，重新烧写不等于恢复出厂；
 * - 校验不通过就不显示"完成"——这里不会把失败包装成"完成，但有警告"。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, asAppError, onFirmwareProgress } from '../lib/ipc'
import {
  ERROR_HINTS,
  type AppError,
  type FirmwareStatus,
  type FlashReport,
  type ProbeResult,
} from '../lib/types'

const status = ref<FirmwareStatus | null>(null)
const probes = ref<Record<string, ProbeResult>>({})
const busy = ref<{ port: string; kind: 'probe' | 'flash' } | null>(null)
const progress = ref(0)
const report = ref<FlashReport | null>(null)
const error = ref<AppError | null>(null)

let unlisten: (() => void) | undefined

const errorHint = computed(() => (error.value ? ERROR_HINTS[error.value.code] : ''))
const canFlash = computed(() => status.value?.image != null && busy.value === null)

async function refresh() {
  try {
    status.value = await api.firmwareStatus()
  } catch (e) {
    error.value = asAppError(e)
  }
}

function stateLabel(port: string): string {
  const p = probes.value[port]
  if (!p) return '未检测'
  switch (p.state) {
    case 'up_to_date':
      return `已是最新 ${p.image_version}`
    case 'blank':
      return '空片，需要首烧'
    case 'mismatch':
      return `设备 ${p.current_version ?? '未知'} → 包内 ${p.image_version}`
    default:
      return '版本未知（可能是旧固件或描述符损坏）'
  }
}

function stateTone(port: string): 'dim' | 'ok' | 'warn' {
  const p = probes.value[port]
  if (!p) return 'dim'
  if (p.state === 'up_to_date') return 'ok'
  return 'warn'
}

async function probe(port: string) {
  if (busy.value) return
  busy.value = { port, kind: 'probe' }
  error.value = null
  try {
    probes.value = { ...probes.value, [port]: await api.firmwareProbe(port) }
  } catch (e) {
    error.value = asAppError(e)
    const { [port]: _drop, ...rest } = probes.value
    probes.value = rest
  } finally {
    busy.value = null
  }
}

async function flash(port: string) {
  if (!canFlash.value) return
  busy.value = { port, kind: 'flash' }
  error.value = null
  report.value = null
  progress.value = 0
  try {
    const done = await api.firmwareFlash(port)
    report.value = done
    // 校验通过 = flash 内容与镜像逐字节一致，版本必然等于镜像版本。
    // 这是推断，不是重新探测——想复核再点一次「检测」。
    probes.value = {
      ...probes.value,
      [port]: {
        port,
        chip: probes.value[port]?.chip ?? 'ESP32-C3',
        current_version: done.to_version,
        image_version: done.to_version,
        state: 'up_to_date',
        first_flash: false,
      },
    }
  } catch (e) {
    error.value = asAppError(e)
  } finally {
    busy.value = null
    progress.value = 0
  }
}

onMounted(async () => {
  await refresh()
  unlisten = await onFirmwareProgress((percent) => {
    progress.value = percent
  })
})

onUnmounted(() => unlisten?.())
</script>

<template>
  <div class="fw">
    <section class="card">
      <div class="card-head">
        <h2>随包固件</h2>
        <span class="spacer" />
        <span v-if="status?.image" class="meta mono">{{ status.image.version }}</span>
      </div>

      <template v-if="status?.image">
        <div class="grid">
          <div class="cell">
            <span>版本</span><b>{{ status.image.version }}</b>
          </div>
          <div class="cell">
            <span>大小</span><b>{{ Math.round(status.image.bytes / 1024) }} KB</b>
          </div>
          <div class="cell">
            <span>app 分区 MD5</span>
            <b class="mono">{{ status.image.md5.slice(0, 16) }}…</b>
          </div>
        </div>
        <p class="path mono">{{ status.image.path }}</p>
      </template>
      <p v-else-if="status?.image_error" class="err">{{ status.image_error }}</p>
      <p v-else class="dim">正在读取安装包内的固件…</p>
    </section>

    <section class="card">
      <div class="card-head">
        <h2>设备</h2>
        <span class="spacer" />
        <button :disabled="busy !== null" @click="refresh">重新扫描</button>
      </div>

      <p v-if="!status?.ports.length" class="dim">
        没检测到设备。用数据线直连电脑（不要经无源 HUB），换个 USB 口再试；
        设备是空片也能识别——靠的是芯片内置的 USB，不是固件。
      </p>

      <ul v-else class="ports">
        <li v-for="p in status?.ports" :key="p.path">
          <div class="port-main">
            <span class="mono">{{ p.path }}</span>
            <span class="tag" :data-tone="stateTone(p.path)">{{ stateLabel(p.path) }}</span>
          </div>

          <div class="actions">
            <template v-if="busy?.port === p.path && busy.kind === 'flash'">
              <div class="bar"><i :style="{ width: `${progress}%` }" /></div>
              <span class="meta mono">{{ progress }}%</span>
            </template>
            <template v-else>
              <button :disabled="busy !== null" @click="probe(p.path)">
                {{ busy?.port === p.path ? '检测中…' : '检测' }}
              </button>
              <button
                class="primary"
                :disabled="!canFlash || probes[p.path]?.state === 'up_to_date'"
                @click="flash(p.path)"
              >
                {{ probes[p.path]?.first_flash ? '写入固件' : '重写固件' }}
              </button>
            </template>
          </div>
        </li>
      </ul>
    </section>

    <div v-if="error" class="result err-box">
      <strong>{{ error.code }}</strong>
      <span>{{ error.detail }}</span>
      <p class="dim">{{ errorHint }}</p>
    </div>

    <div v-if="report" class="result ok-box">
      <strong>写入完成，校验通过</strong>
      <span class="mono">
        {{ report.from_version ?? '空片' }} → {{ report.to_version }} ·
        {{ Math.round(report.bytes_written / 1024) }} KB ·
        {{ (report.elapsed_ms / 1000).toFixed(1) }} s
      </span>
      <p class="dim mono">app 分区 MD5 {{ report.md5 }}</p>
      <p class="dim">设备已复位，接下来去「实时」页扫描并连接。</p>
    </div>

    <p class="foot dim">
      烧写会复位设备，正在推流时不要点。只写 bootloader / 分区表 / app 三段，
      <b>不碰 NVS</b>——配对信息和按键映射会保留。中途拔线不会刷坏设备：重新插上再点一次即可。
    </p>
  </div>
</template>

<style scoped>
.fw {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 12px 16px 16px;
  overflow-y: auto;
  min-height: 0;
}

.card {
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.card-head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.card-head h2 {
  font-size: 12.5px;
  font-weight: 600;
  margin: 0;
}

.spacer {
  flex: 1;
}

.meta {
  font-size: 11px;
  color: var(--muted);
}

.grid {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 6px;
}

.cell {
  background: var(--panel-2);
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
  color: var(--dim);
}

.cell b {
  font-size: 12.5px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.path {
  font-size: 10.5px;
  color: var(--dim);
  margin: 0;
  word-break: break-all;
}

.ports {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.ports li {
  display: flex;
  align-items: center;
  gap: 10px;
  background: var(--panel-2);
  border: 1px solid var(--line-soft);
  border-radius: 8px;
  padding: 8px 10px;
}

.port-main {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  flex: 1;
}

.port-main .mono {
  font-size: 11.5px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.tag {
  font-size: 11px;
}

.tag[data-tone='ok'] {
  color: var(--ok);
}

.tag[data-tone='warn'] {
  color: var(--warn);
}

.tag[data-tone='dim'] {
  color: var(--dim);
}

.actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}

.bar {
  width: 120px;
  height: 6px;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 3px;
  overflow: hidden;
}

.bar i {
  display: block;
  height: 100%;
  background: var(--accent);
  transition: width 0.15s linear;
}

.result {
  border-radius: 8px;
  border: 1px solid var(--line);
  border-left-width: 3px;
  padding: 8px 10px;
  display: flex;
  flex-direction: column;
  gap: 3px;
  font-size: 12px;
}

.result strong {
  font-size: 12.5px;
}

.result p {
  margin: 0;
  font-size: 11px;
  line-height: 1.6;
}

.err-box {
  border-left-color: var(--err);
  background: #1f1518;
}

.ok-box {
  border-left-color: var(--ok);
  background: #121d18;
}

.err {
  color: var(--err);
  font-size: 11.5px;
  margin: 0;
}

.dim {
  color: var(--muted);
  font-size: 11.5px;
  line-height: 1.65;
  margin: 0;
}

.foot {
  font-size: 11px;
  line-height: 1.7;
  margin: 2px 0 0;
}

.foot b {
  color: var(--text);
  font-weight: 600;
}

.mono {
  font-family: var(--mono);
}

button.primary {
  background: var(--accent);
  border-color: var(--accent);
  color: #fff;
}

button.primary:hover:not(:disabled) {
  background: #5c98ff;
  border-color: #5c98ff;
}
</style>
