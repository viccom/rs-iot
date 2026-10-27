// frontend/vite.config.ts
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

export default defineConfig({
  plugins: [vue()],
  server: {
    proxy: {
      // dev：前端 :5173，API 经 proxy 到 gateway :8080（同源，无 CORS）。
      // SSE 适配：prod build 经 gateway 直连验证实时性 OK（T5/T11）；dev 走 vite proxy
      // 若发现日志/报文 SSE 卡顿，加 configure 设 Accept: text/event-stream 头提示上游按 SSE 连接处理：
      //   configure: (proxy) => proxy.on('proxyReq', (req) => req.setHeader('Accept', 'text/event-stream'))
      //（当前不加——YAGNI，dev 模式未实测到缓冲问题；configure 是全局的会影响非 SSE 的 REST 请求）
      '/api': { target: 'http://127.0.0.1:8080', changeOrigin: true },
    },
  },
  build: { outDir: 'dist' },
})
