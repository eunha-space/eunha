import { expect, test } from '@playwright/test'

const account = {
  id: '1',
  username: 'alice',
  acct: 'alice',
  display_name: 'Alice',
  note: '',
  url: 'https://example.invalid/@alice',
  uri: 'https://example.invalid/users/alice',
  avatar: '',
  avatar_static: '',
  header: '',
  header_static: '',
  followers_count: 0,
  following_count: 0,
  statuses_count: 2,
  created_at: '2026-01-01T00:00:00.000Z',
  last_status_at: null,
  emojis: [],
  fields: [],
  locked: false,
  bot: false,
  group: false,
  discoverable: true,
  indexable: true,
}

const status = (id: string, content: string, pinned: boolean) => ({
  id,
  created_at: '2026-01-01T00:00:00.000Z',
  in_reply_to_id: null,
  in_reply_to_account_id: null,
  sensitive: false,
  spoiler_text: '',
  visibility: 'public',
  language: 'en',
  uri: `https://example.invalid/users/alice/statuses/${id}`,
  url: `https://example.invalid/@alice/${id}`,
  replies_count: 0,
  reblogs_count: 0,
  favourites_count: 0,
  edited_at: null,
  content: `<p>${content}</p>`,
  reblog: null,
  account,
  media_attachments: [],
  mentions: [],
  tags: [],
  emojis: [],
  card: null,
  poll: null,
  favourited: false,
  reblogged: false,
  muted: false,
  bookmarked: false,
  pinned,
})

// `?pinned=true` is a separate request from the timeline, so the section can
// only be right if the page asks for it — and the profile of someone with no
// pins must not grow an empty heading.
test('a profile shows pinned posts above the timeline', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/lookup**', (route) => route.fulfill({ json: account }))
  await page.route('**/api/v1/accounts/verify_credentials**', (route) =>
    route.fulfill({ json: account }),
  )
  await page.route('**/api/v1/accounts/relationships**', (route) => route.fulfill({ json: [] }))
  // Playwright matches routes newest-first, so the pinned form is registered
  // after the general one to win for the request that carries the flag.
  await page.route('**/api/v1/accounts/1/statuses**', (route) =>
    route.fulfill({ json: [status('2', 'ordinary post', false)] }),
  )
  await page.route('**/api/v1/accounts/1/statuses?pinned=true**', (route) =>
    route.fulfill({ json: [status('1', 'the pinned one', true)] }),
  )

  await page.goto('/@alice')

  const pinned = page.locator('section', { hasText: 'Pinned' })
  await expect(pinned).toContainText('the pinned one')
  await expect(pinned).not.toContainText('ordinary post')
})

test('a profile with no pins has no pinned heading', async ({ page }) => {
  await page.route('**/api/v1/accounts/lookup**', (route) => route.fulfill({ json: account }))
  await page.route('**/api/v1/accounts/1/statuses**', (route) =>
    route.fulfill({ json: [status('2', 'ordinary post', false)] }),
  )
  await page.route('**/api/v1/accounts/1/statuses?pinned=true**', (route) =>
    route.fulfill({ json: [] }),
  )

  await page.goto('/@alice')
  await expect(page.getByText('ordinary post')).toBeVisible()
  await expect(page.getByText('Pinned', { exact: true })).toHaveCount(0)
})

// The cap of five is the server's, and its 422 names the reason — the card
// shows what it says rather than inventing a message.
test('the pin cap is reported in the server’s own words', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/lookup**', (route) => route.fulfill({ json: account }))
  await page.route('**/api/v1/accounts/verify_credentials**', (route) =>
    route.fulfill({ json: account }),
  )
  await page.route('**/api/v1/accounts/relationships**', (route) => route.fulfill({ json: [] }))
  await page.route('**/api/v1/accounts/1/statuses**', (route) =>
    route.fulfill({ json: [status('2', 'ordinary post', false)] }),
  )
  await page.route('**/api/v1/accounts/1/statuses?pinned=true**', (route) =>
    route.fulfill({ json: [] }),
  )
  await page.route('**/api/v1/statuses/2/pin', (route) =>
    route.fulfill({
      status: 422,
      json: {
        error: 'Validation failed: You have already pinned the maximum number of statuses',
      },
    }),
  )

  await page.goto('/@alice')
  await page.getByRole('button', { name: 'More' }).first().click()
  await page.getByRole('menuitem', { name: 'Pin to profile' }).click()

  await expect(
    page.getByText(/You have already pinned the maximum number of statuses/),
  ).toBeVisible()
})
