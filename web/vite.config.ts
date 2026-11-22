import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import type { Plugin } from 'vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'
import pkg from './package.json' with { type: 'json' }

const backend = process.env.LYNX_BACKEND ?? 'http://localhost:8080'

// Same shape as nginx in the image: /backend/* goes to the Rust server without the prefix.
const backendProxy = {
  '/backend': {
    target: backend,
    ws: true,
    rewrite: (path: string) => path.replace(/^\/backend/, ''),
  },
}

// `pnpm dev`       -> real backend through the dev proxy.
// `pnpm dev:mock`  -> `--mode mock`: the Discord SDK is aliased to a local mock and a fake API runs
//                    inside the dev server. Nothing under mock/ is reachable from a production build.
// Same rule as nginx in the image: pages are only served inside Discord (the Activity iframe always
// carries ?frame_id=...). A direct visit gets dist/unauthorized.html (status 200). Assets and /backend pass.
function discordOnly(): Plugin {
  return {
    name: 'lynx-discord-only',
    configurePreviewServer(server) {
      server.middlewares.use((req, res, next) => {
        const url = new URL(req.url ?? '/', 'http://localhost')
        const isPage = url.pathname === '/' || url.pathname === '/index.html' || !/\.[a-z0-9]+$/i.test(url.pathname)
        if (!isPage || url.pathname.startsWith('/backend') || url.searchParams.has('frame_id')) return next()
        res.statusCode = 200
        res.setHeader('Content-Type', 'text/html; charset=utf-8')
        res.setHeader('Cache-Control', 'no-cache')
        res.end(readFileSync(fileURLToPath(new URL('./dist/unauthorized.html', import.meta.url))))
      })
    },
  }
}

export default defineConfig(async ({ mode }) => {
  const mock = mode === 'mock'
  const alias: Record<string, string> = mock
    ? { '@discord/embedded-app-sdk': fileURLToPath(new URL('./mock/discord-sdk.ts', import.meta.url)) }
    : {}
  return {
    base: '/',
    define: { __APP_VERSION__: JSON.stringify(pkg.version) },
    // Imported lazily: the mock keeps timers alive, which would stop `vite build` from exiting.
    plugins: [react(), discordOnly(), ...(mock ? [(await import('./mock/server')).mockApi()] : [])],
    resolve: { alias },
    build: { outDir: 'dist', assetsDir: 'assets', emptyOutDir: true, chunkSizeWarningLimit: 700 },
    server: mock ? {} : { proxy: backendProxy },
    // Testing inside Discord: `vite build --watch` + `vite preview` (scripts/dev-windows.ps1) serves the
    // built panel like nginx does in the image. No HMR: a reloaded Activity iframe never gets the SDK
    // handshake again, and Tailwind's dev CSS only grows through HMR.
    preview: { allowedHosts: true as const, proxy: backendProxy, headers: { 'Cache-Control': 'no-cache' } },
  }
})
