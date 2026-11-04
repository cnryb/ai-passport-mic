<script setup lang="ts">
import type { DeviceView } from '../lib/types'

defineProps<{
  devices: DeviceView[]
  connectedId: string | null
  busy: boolean
  scanning: boolean
}>()

const emit = defineEmits<{ (e: 'connect', id: string): void }>()

function rssiTone(rssi: number | null): string {
  if (rssi === null) return 'unknown'
  if (rssi >= -55) return 'strong'
  if (rssi >= -70) return 'ok'
  return 'weak'
}

function shortId(id: string): string {
  return id.length > 12 ? `${id.slice(0, 8)}…` : id
}
</script>

<template>
  <div class="list scroll">
    <p v-if="devices.length === 0" class="empty">
      <template v-if="scanning">正在扫描…设备会在发现后立刻出现</template>
      <template v-else>还没有设备。点「扫描」开始；设备需上电并且未被其他主机连接。</template>
    </p>

    <div
      v-for="d in devices"
      :key="d.id"
      class="row"
      :data-connected="d.id === connectedId"
    >
      <div class="id">
        <span class="name">{{ d.name || '（未命名设备）' }}</span>
        <span class="sub mono">{{ shortId(d.id) }}</span>
      </div>

      <span v-if="d.has_service" class="badge">本项目</span>
      <span v-else class="badge muted">其他</span>

      <span class="rssi" :data-tone="rssiTone(d.rssi)">
        <i class="bars"><b /><b /><b /></i>
        {{ d.rssi === null ? '—' : `${d.rssi} dBm` }}
      </span>

      <button
        v-if="d.id === connectedId"
        class="ghost"
        disabled
      >
        已连接
      </button>
      <button
        v-else
        class="primary"
        :disabled="busy || connectedId !== null"
        @click="emit('connect', d.id)"
      >
        连接
      </button>
    </div>
  </div>
</template>

<style scoped>
.list {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-height: 0;
}

.empty {
  color: var(--dim);
  font-size: 12px;
  padding: 18px 4px;
  margin: 0;
  line-height: 1.7;
}

.row {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto auto auto;
  align-items: center;
  gap: 10px;
  padding: 9px 10px;
  border-radius: 8px;
  border: 1px solid transparent;
}

.row:hover {
  background: #171d26;
  border-color: var(--line-soft);
}

.row[data-connected='true'] {
  background: #3ddc970f;
  border-color: #3ddc9733;
}

.id {
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 500;
}

.sub {
  color: var(--dim);
}

.badge {
  font-size: 11px;
  padding: 1px 7px;
  border-radius: 999px;
  background: #4c8dff1f;
  color: #9ec3ff;
  white-space: nowrap;
}

.badge.muted {
  background: #ffffff0a;
  color: var(--dim);
}

.rssi {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 11.5px;
  color: var(--muted);
  font-variant-numeric: tabular-nums;
  min-width: 74px;
  justify-content: flex-end;
}

.bars {
  display: inline-flex;
  align-items: flex-end;
  gap: 2px;
  height: 11px;
}

.bars b {
  width: 3px;
  background: var(--dim);
  border-radius: 1px;
}

.bars b:nth-child(1) {
  height: 4px;
}

.bars b:nth-child(2) {
  height: 7px;
}

.bars b:nth-child(3) {
  height: 11px;
}

.rssi[data-tone='strong'] .bars b {
  background: var(--ok);
}

.rssi[data-tone='ok'] .bars b:nth-child(1),
.rssi[data-tone='ok'] .bars b:nth-child(2) {
  background: var(--ok);
}

.rssi[data-tone='weak'] .bars b:nth-child(1) {
  background: var(--warn);
}

.rssi[data-tone='unknown'] .bars {
  opacity: 0.35;
}

button {
  min-width: 62px;
}
</style>
