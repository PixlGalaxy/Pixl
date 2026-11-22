import { fileURLToPath } from 'node:url'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'
import pkg from './package.json' with { type: 'json' }

const backend = process.env.LYNX_BACKEND ?? 'http://localhost:8080'

// `pnpm dev`       -> real backend through the dev proxy.
// `pnpm dev:mock`  -> `--mode mock`: the Discord SDK is aliased to a local mock and a fake API runs
//                    inside the dev server. Nothing under mock/ is reachable from a production build.
export default defineConfig(async ({ mode }) => {
  const mock = mode === 'mock'
  const alias: Record<string, string> = mock
    ? { '@discord/embedded-app-sdk': fileURLToPath(new URL('./mock/discord-sdk.ts', import.meta.url)) }
    : {}
  return {
    base: '/',
    define: { __APP_VERSION__: JSON.stringify(pkg.version) },
    // Imported lazily: the mock keeps timers alive, which would stop `vite build` from exiting.
    plugins: [react(), ...(mock ? [(await import('./mock/server')).mockApi()] : [])],
    resolve: { alias },
    build: { outDir: 'dist', assetsDir: 'assets', emptyOutDir: true, chunkSizeWarningLimit: 700 },
    server: mock
      ? {}
      : {
          // Dev only: accept the public host NPM forwards (the Activity runs through Discord's proxy).
          allowedHosts: true,
          // Same shape as nginx in the image: /backend/* goes to the Rust server without the prefix.
          proxy: {
            '/backend': {
              target: backend,
              ws: true,
              rewrite: (path: string) => path.replace(/^\/backend/, ''),
            },
          },
        },
  }
})
