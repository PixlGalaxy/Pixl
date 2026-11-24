import { fileURLToPath } from 'node:url'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'
import pkg from './package.json' with { type: 'json' }

// Operator admin panel (port 8081 in the image): its own entry and output folder, so nothing of it is
// served by the Activity (dist/) and it never bundles the Discord SDK. `pnpm build` builds both.
// The API lives on the same origin under /api (nginx proxies it to the admin listener; in the Windows
// dev run the bot serves dist-admin/ and the API itself on 127.0.0.1:8081).
const backend = process.env.LYNX_ADMIN_BACKEND ?? 'http://127.0.0.1:8081'

export default defineConfig({
  base: '/',
  define: { __APP_VERSION__: JSON.stringify(pkg.version) },
  plugins: [react()],
  publicDir: false,
  build: {
    outDir: 'dist-admin',
    assetsDir: 'assets',
    emptyOutDir: true,
    rollupOptions: { input: fileURLToPath(new URL('./admin.html', import.meta.url)) },
  },
  // `pnpm dev:admin`: live UI against a running bot's admin listener.
  server: { port: 5174, proxy: { '/api': { target: backend } } },
})
