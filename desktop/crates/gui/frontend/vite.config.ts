import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    // WKWebView（macOS 11+）是唯一的运行目标，不用为老浏览器降级。
    target: 'safari15',
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: true,
  },
})
