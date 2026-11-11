import { execSync } from 'node:child_process'
import path from 'node:path'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// Short hash of the eunha commit this SPA was built from. Frontend and backend
// live in the same repo, so this identifies the running server build.
function commitHash() {
  try {
    return execSync('git rev-parse --short HEAD').toString().trim()
  } catch {
    return 'unknown'
  }
}

// The Rust server (axum) serves the built SPA from `frontend/dist`. In dev we
// proxy the C2S API and OAuth endpoints to the running eunha server on :3000.
export default defineConfig({
  define: {
    __COMMIT_HASH__: JSON.stringify(commitHash()),
  },
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  build: {
    outDir: 'dist',
  },
  server: {
    proxy: {
      '/api': 'http://localhost:3000',
      '/oauth': 'http://localhost:3000',
      '/.well-known': 'http://localhost:3000',
    },
  },
})
