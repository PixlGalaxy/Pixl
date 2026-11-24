import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'
import pkg from './package.json' with { type: 'json' }

// Lynx operator admin panel: its own app, served on port 8081 (nginx in the image; the bot itself in
// the Windows dev run). The API is on the same origin under /api. No Discord SDK here.
// `pnpm dev` runs the UI with live reload against a running bot's admin listener.
const backend = process.env.LYNX_ADMIN_BACKEND ?? 'http://127.0.0.1:8081'

export default defineConfig({
  base: '/',
  define: { __APP_VERSION__: JSON.stringify(pkg.version) },
  plugins: [react()],
  build: { outDir: 'dist', assetsDir: 'assets', emptyOutDir: true },
  server: { port: 5174, strictPort: true, proxy: { '/api': { target: backend } } },
})
