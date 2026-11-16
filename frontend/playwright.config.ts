import { defineConfig, devices } from '@playwright/test'

// Client-only smoke tests. They run against the Vite dev server; API calls fail
// without a backend, but the surfaces under test (theme switcher, header) are
// purely client-side.
export default defineConfig({
  testDir: './e2e',
  fullyParallel: true,
  use: {
    baseURL: 'http://localhost:5173',
    trace: 'on-first-retry',
    // The tests find things by their English names; the interface follows the
    // browser's language when nothing else picks one (see src/i18n.tsx).
    locale: 'en-US',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
  webServer: {
    command: 'npm run dev',
    url: 'http://localhost:5173',
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
  },
})
