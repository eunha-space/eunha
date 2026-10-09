import { expect, test, type Page } from '@playwright/test'

// `/api/v2/instance` with the live feed access settings in
// `configuration.timelines_access`.
async function liveFeedAccess(page: Page, remote: string) {
  await page.route('**/api/v2/instance', (r) => r.fulfill({
    json: {
      domain: 'community.example',
      title: 'Community',
      icon: [],
      registrations: { enabled: false },
      configuration: {
        translation: { enabled: false },
        timelines_access: { live_feeds: { local: 'public', remote } },
      },
    },
  }))
}

test('the sidebar retains its domain while instance details refresh after navigation', async ({ page }) => {
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v2/instance', (r) => r.fulfill({
    json: { domain: 'community.example', title: 'Community', icon: [], registrations: { enabled: false } },
  }))
  await page.goto('/')
  const domain = page.locator('aside.sidebar-frame').getByText('community.example', { exact: true })
  await expect(domain).toBeVisible()

  // Hold the refresh open: a remounted sidebar must render its cached domain
  // before the request finishes, including when that request fails.
  let release!: () => void
  const held = new Promise<void>((resolve) => { release = resolve })
  await page.route('**/api/v2/instance', async (r) => {
    await held
    await r.abort()
  })
  const refresh = page.waitForRequest('**/api/v2/instance')
  await page.locator('aside.sidebar-frame').getByRole('link', { name: 'Local' }).click()
  await refresh
  await expect(page).toHaveURL(/\/local$/)
  await expect(domain).toBeVisible()
  release()
  await expect(domain).toBeVisible()
})

// The home and local timelines used to be a tab strip inside the column. They are rows
// in the rail now, which is the whole point of the change — so the test is
// that they navigate from there, and that the strip is gone.
test('the rail carries the timelines, and no tab strip remains', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/accounts/verify_credentials**', (r) =>
    r.fulfill({
      json: {
        id: '1',
        acct: 'alice',
        username: 'alice',
        display_name: 'Alice',
        avatar: '',
        source: { privacy: 'public' },
      },
    }),
  )
  await page.route('**/api/v1/notifications/unread_count**', (r) =>
    r.fulfill({ json: { count: 0 } }),
  )
  await liveFeedAccess(page, 'authenticated')

  await page.goto('/')
  const rail = page.locator('aside')
  for (const label of ['Home', 'Local', 'Federated', 'Messages', 'Saved']) {
    await expect(rail.getByRole('link', { name: label })).toBeVisible()
  }

  await rail.getByRole('link', { name: 'Local' }).click()
  await expect(page).toHaveURL(/\/local$/)
  await rail.getByRole('link', { name: 'Federated' }).click()
  await expect(page).toHaveURL(/\/public$/)
})

// Signed out, "/" *is* the local timeline, so that row has to own both paths
// or a visitor lands on a page with nothing lit.
test('signed out, the local row owns the root path', async ({ page }) => {
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [] }))
  await liveFeedAccess(page, 'public')
  await page.goto('/')

  await expect(page.locator('aside').getByRole('link', { name: 'Federated' })).toBeVisible()
  const local = page.locator('aside').getByRole('link', { name: 'Local' })
  await expect(local).toHaveClass(/bg-muted/)
  // And there is still a way to change the theme without an account menu.
  await expect(page.getByRole('button', { name: 'Toggle theme' })).toBeVisible()
})

// Mastodon's navigation offers a live feed only to whoever may read it:
// `authenticated` keeps the federated timeline from visitors.
test('signed out, the federated row follows remote_live_feed_access', async ({ page }) => {
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [] }))
  await liveFeedAccess(page, 'authenticated')
  await page.goto('/')

  const rail = page.locator('aside')
  await expect(rail.getByRole('link', { name: 'Local' })).toBeVisible()
  await expect(rail.getByRole('link', { name: 'Federated' })).toHaveCount(0)
})
