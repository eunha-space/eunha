import { expect, test } from '@playwright/test'

const account = {
  id: '2',
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

const version = (content: string, createdAt: string, spoiler = '') => ({
  content,
  spoiler_text: spoiler,
  sensitive: false,
  created_at: createdAt,
  account,
  media_attachments: [],
  emojis: [],
})

// The server returns versions oldest first and appends the current one, so the
// page has to turn that round: newest at the top, and the labels have to name
// the right ends of the list.
test('edit history reads newest first, with the ends named', async ({ page }) => {
  await page.route('**/api/v1/statuses/*/history', (route) =>
    route.fulfill({
      json: [
        version('<p>first</p>', '2026-01-01T00:00:00.000Z'),
        version('<p>second</p>', '2026-01-02T00:00:00.000Z'),
        version('<p>third</p>', '2026-01-03T00:00:00.000Z', 'A warning'),
      ],
    }),
  )

  await page.goto('/@bob/1/history')

  const labels = page.locator('li .font-semibold')
  await expect(labels).toHaveText(['Current version', 'Version 2', 'Original'])

  // Newest first means the current version's text leads.
  const items = page.locator('li')
  await expect(items.first()).toContainText('third')
  await expect(items.first()).toContainText('A warning')
  await expect(items.last()).toContainText('first')
})

test('a status with no edits shows its single current version', async ({ page }) => {
  await page.route('**/api/v1/statuses/*/history', (route) =>
    route.fulfill({ json: [version('<p>only ever this</p>', '2026-01-01T00:00:00.000Z')] }),
  )

  await page.goto('/@bob/1/history')
  await expect(page.locator('li .font-semibold')).toHaveText(['Current version'])
  await expect(page.locator('li')).toContainText('only ever this')
})

// Mastodon's history modal lists each version's poll options as they read.
test('each version shows its poll options as they were', async ({ page }) => {
  const blob = { shortcode: 'blob', url: '/emoji/blob.png', static_url: '/emoji/blob.png', visible_in_picker: true }
  await page.route('**/api/v1/statuses/*/history', (route) =>
    route.fulfill({
      json: [
        { ...version('<p>vote</p>', '2026-01-01T00:00:00.000Z'), poll: { options: [{ title: 'Tea' }, { title: 'Cofee' }] } },
        { ...version('<p>vote</p>', '2026-01-02T00:00:00.000Z'), emojis: [blob],
          poll: { options: [{ title: 'Tea :blob:' }, { title: 'Coffee' }, { title: 'Water' }] } },
      ],
    }),
  )
  await page.route('**/emoji/*', (r) => r.fulfill({
    contentType: 'image/svg+xml',
    body: '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#c44" /></svg>',
  }))

  await page.goto('/@bob/1/history')
  const items = page.locator('li')
  const current = items.first().getByRole('list', { name: 'Poll options' }).getByRole('listitem')
  await expect(current).toHaveText(['Tea', 'Coffee', 'Water'])
  await expect(current.first().locator('img.custom-emoji')).toHaveAttribute('alt', ':blob:')
  const original = items.last().getByRole('list', { name: 'Poll options' }).getByRole('listitem')
  await expect(original).toHaveText(['Tea', 'Cofee'])
})
