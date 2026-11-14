import { expect, test } from '@playwright/test'

const bob = {
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
}

// Coming back to Notifications from the sidebar should show what was there a
// moment ago while the fresh request is out, as the timelines do, rather than
// blanking the column to "Loading…".
test('notifications show what they held last while reloading', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/markers', r => r.fulfill({ json: {} }))
  await page.route('**/api/v1/timelines/**', r => r.fulfill({ json: [] }))
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
  await page.route('**/api/v1/notifications**', r =>
    r.fulfill({ json: [{ id: '42', type: 'follow', created_at: '2026-01-01T00:00:00.000Z', account: bob }] }),
  )
  await page.route('**/api/v1/notifications/unread_count**', r => r.fulfill({ json: { count: 0 } }))

  await page.goto('/notifications')
  await expect(page.getByText('followed you')).toBeVisible()

  await page.locator('aside').getByRole('link', { name: 'Home', exact: true }).first().click()
  await expect(page).not.toHaveURL(/\/notifications$/)
  await expect(page.getByText('followed you')).toHaveCount(0)

  let release!: () => void
  const held = new Promise<void>(resolve => { release = resolve })
  await page.route('**/api/v1/notifications?**', async r => {
    await held
    await r.fulfill({ json: [] })
  })
  const refetch = page.waitForRequest('**/api/v1/notifications?**')
  await page.locator('aside').getByRole('link', { name: 'Notifications' }).first().click()
  await refetch
  await expect(page.getByText('followed you')).toBeVisible()
  await expect(page.getByText('Loading…')).toHaveCount(0)
  release()
  await expect(page.getByText('No notifications yet.')).toBeVisible()
})
