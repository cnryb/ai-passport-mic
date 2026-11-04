<script setup lang="ts">
import { computed } from 'vue'
import { STATE_LABEL, type AppState } from '../lib/types'

const props = defineProps<{ state: AppState }>()

const tone = computed(() => {
  switch (props.state.kind) {
    case 'ready':
    case 'streaming':
      return 'ok'
    case 'scanning':
    case 'connecting':
    case 'stopping':
      return 'busy'
    case 'error':
      return 'err'
    case 'no_adapter':
      return 'warn'
    default:
      return 'idle'
  }
})
</script>

<template>
  <span class="pill" :data-tone="tone">
    <i class="dot" />
    {{ STATE_LABEL[state.kind] }}
  </span>
</template>

<style scoped>
.pill {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  padding: 4px 11px 4px 9px;
  border-radius: 999px;
  font-size: 12px;
  font-weight: 500;
  border: 1px solid var(--line);
  background: var(--panel-2);
  white-space: nowrap;
}

.dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--dim);
}

.pill[data-tone='ok'] {
  color: var(--ok);
  border-color: #3ddc9740;
  background: #3ddc9714;
}

.pill[data-tone='ok'] .dot {
  background: var(--ok);
  box-shadow: 0 0 0 3px #3ddc9722;
}

.pill[data-tone='busy'] {
  color: #9ec3ff;
  border-color: #4c8dff40;
  background: #4c8dff14;
}

.pill[data-tone='busy'] .dot {
  background: var(--accent);
  animation: pulse 1.1s ease-in-out infinite;
}

.pill[data-tone='warn'] {
  color: var(--warn);
  border-color: #ffb02040;
  background: #ffb02014;
}

.pill[data-tone='warn'] .dot {
  background: var(--warn);
}

.pill[data-tone='err'] {
  color: var(--err);
  border-color: #ff5f5f40;
  background: #ff5f5f14;
}

.pill[data-tone='err'] .dot {
  background: var(--err);
}

@keyframes pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.35;
  }
}
</style>
