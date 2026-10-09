import { expect, test, type Page } from '@playwright/test'

// The server's announcements, as Mastodon's home column shows them: behind a
// badged button, newest first, with their custom emoji and reactions drawn.
// Every request is stubbed.
const blob = { shortcode: 'blob', url: '/emoji/blob.gif', static_url: '/emoji/blob.png', visible_in_picker: true }
const me = {
  id: '1', username: 'alice', acct: 'alice', display_name: 'Alice', note: '',
  url: 'https://example.invalid/@alice', uri: 'https://example.invalid/users/alice',
  avatar: '', avatar_static: '', header: '', header_static: '', emojis: [], fields: [],
  followers_count: 0, following_count: 0, statuses_count: 0,
  created_at: '2026-01-01T00:00:00Z', locked: false, bot: false, group: false,
}
const announcement = (id: string, content: string, extra: Record<string, unknown> = {}) => ({
  id, content, starts_at: null, ends_at: null, all_day: false,
  published_at: '2026-09-01T12:00:00Z', updated_at: '2026-09-01T12:00:00Z', read: false,
  mentions: [], statuses: [], tags: [], emojis: [], reactions: [], ...extra,
})

async function setup(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/verify_credentials**', (r) => r.fulfill({ json: {
    ...me, source: { privacy: 'public' }, role: { id: '1', name: '', permissions: '0' },
  } }))
  await page.route('**/api/v1/notifications/unread_count**', (r) => r.fulfill({ json: { count: 0 } }))
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/custom_emojis', (r) => r.fulfill({ json: [blob] }))
  await page.route(/\/emoji\//, (r) => r.fulfill({
    contentType: 'image/svg+xml',
    body: '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#c44" /></svg>',
  }))
  const calls: string[] = []
  await page.route('**/api/v1/announcements**', (r) => {
    const url = new URL(r.request().url())
    if (url.pathname === '/api/v1/announcements') {
      return r.fulfill({ json: [
        announcement('1', '<p>Older news</p>'),
        announcement('2', '<p>Maintenance :blob: tonight :nope:</p>', {
          emojis: [blob],
          reactions: [
            { name: 'blob', count: 2, me: false, url: '/emoji/blob.gif', static_url: '/emoji/blob.png' },
            { name: '👍', count: 1, me: true },
          ],
        }),
      ] })
    }
    calls.push(`${r.request().method()} ${decodeURIComponent(url.pathname)}`)
    return r.fulfill({ json: {} })
  })
  await page.goto('/')
  return calls
}

test('announcements open from the home column, newest first, with their emoji', async ({ page }) => {
  const calls = await setup(page)
  const toggle = page.getByRole('button', { name: 'Show announcements' })
  await expect(toggle).toContainText('2')
  await toggle.click()
  const panel = page.getByRole('region', { name: 'Announcements' })
  await expect(panel.getByText('Maintenance')).toBeVisible()
  await expect(panel.getByText('1 / 2')).toBeVisible()
  const emoji = panel.getByText('Maintenance').locator('img.custom-emoji')
  await expect(emoji).toHaveAttribute('alt', ':blob:')
  await expect(emoji).toHaveAttribute('src', /\/emoji\/blob\.png$/)
  await expect(panel.getByText('Maintenance')).toContainText(':nope:')
  // The one on screen is marked read.
  await expect.poll(() => calls).toContain('POST /api/v1/announcements/2/dismiss')
  expect(calls).not.toContain('POST /api/v1/announcements/1/dismiss')

  // A custom reaction is its image, a Unicode one its character.
  const custom = panel.getByRole('button', { name: /:blob:/ })
  await expect(custom.locator('img')).toHaveAttribute('src', /\/emoji\/blob\.png$/)
  await expect(custom).toContainText('2')
  await expect(panel.getByRole('button', { name: /👍/ })).toHaveAttribute('aria-pressed', 'true')
  await custom.click()
  await expect(custom).toContainText('3')
  await expect(custom).toHaveAttribute('aria-pressed', 'true')
  await expect.poll(() => calls).toContain('PUT /api/v1/announcements/2/reactions/blob')

  // Hovering the announcement animates its emoji.
  await panel.getByText('Maintenance').hover()
  await expect(emoji).toHaveAttribute('src', /\/emoji\/blob\.gif$/)
  await expect(custom.locator('img')).toHaveAttribute('src', /\/emoji\/blob\.gif$/)
})
