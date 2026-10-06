import { expect, test } from '@playwright/test'

// The badge is only useful if it clears, and it clears because the
// notifications page moves the marker the server counts from. So the test that
// matters is not "a number appears" but "reading moves the marker" — without
// that write the count would only ever grow.
test('the notification badge shows a count, and reading marks the timeline', async ({
  page,
}) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })

  let marked: Record<string, unknown> | null = null
  let unread = 3

  await page.route('**/api/v1/markers', async (route) => {
    if (route.request().method() === 'POST') {
      marked = route.request().postDataJSON()
      unread = 0
      await route.fulfill({ json: {} })
      return
    }
    await route.fulfill({ json: {} })
  })
  await page.route('**/api/v1/timelines/public**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/notifications**', (r) =>
    r.fulfill({
      json: [
        {
          id: '42',
          type: 'follow',
          created_at: '2026-01-01T00:00:00.000Z',
          account: {
            id: '3',
            username: 'bob',
            acct: 'bob',
            display_name: 'Bob',
            note: '',
            url: 'https://example.invalid/@bob',
            uri: 'https://example.invalid/users/bob',
            avatar: '',
            avatar_static: '',
            header: '',
            header_static: '',
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            created_at: '2026-01-01T00:00:00.000Z',
            last_status_at: null,
            emojis: [],
            fields: [],
            locked: false,
            bot: false,
            group: false,
          },
        },
      ],
    }),
  )

  // Registered last so it wins over the broad notifications route above:
  // Playwright matches routes newest-first.
  await page.route('**/api/v1/notifications/unread_count**', (r) =>
    r.fulfill({ json: { count: unread } }),
  )

  await page.goto('/local')
  await expect(page.getByLabel('3 unread').first()).toBeVisible()

  await page.getByRole('link', { name: 'Notifications' }).first().click()
  await expect(page.getByText('followed you')).toBeVisible()

  // The marker is moved to the newest notification on screen.
  await expect.poll(() => marked).toEqual({ notifications: { last_read_id: '42' } })
  await expect(page.getByLabel(/unread/)).toHaveCount(0)
})


test('notification count survives a remount and a failed refresh', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/timelines/**', r => r.fulfill({ json: [] }))
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
  await page.route('**/api/v1/notifications/unread_count**', r => r.fulfill({ json: { count: 3 } }))
  await page.goto('/local')
  const badge = page.locator('aside').getByLabel('3 unread')
  await expect(badge).toBeVisible()
  let release!: () => void
  const held = new Promise<void>(resolve => { release = resolve })
  await page.route('**/api/v1/notifications/unread_count**', async r => {
    await held
    await r.fulfill({ status: 500 })
  })
  const refresh = page.waitForRequest('**/api/v1/notifications/unread_count**')
  await page.locator('aside').getByRole('link', { name: 'Search', exact: true }).click()
  await refresh
  await expect(badge).toBeVisible()
  const failed = page.waitForResponse(r => r.url().includes('/notifications/unread_count') && r.status() === 500)
  release()
  await failed
  await expect(badge).toBeVisible()
})
