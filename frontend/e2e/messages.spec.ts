import { expect, test } from '@playwright/test'

const account = (id: string, acct: string, name: string) => ({
  id,
  username: acct.split('@')[0],
  acct,
  display_name: name,
  note: '',
  url: `https://example.invalid/@${acct}`,
  uri: `https://example.invalid/users/${acct}`,
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
  discoverable: true,
  indexable: true,
})

const me = account('1', 'alice', 'Alice')
const bob = account('3', 'bob', 'Bob')

const status = (id: string, content: string, visibility: string, who = bob) => ({
  id,
  created_at: '2026-01-01T00:00:00.000Z',
  in_reply_to_id: null,
  in_reply_to_account_id: null,
  sensitive: false,
  spoiler_text: '',
  visibility,
  language: 'en',
  uri: `https://example.invalid/users/${who.acct}/statuses/${id}`,
  url: `https://example.invalid/@${who.acct}/${id}`,
  replies_count: 0,
  reblogs_count: 0,
  favourites_count: 0,
  edited_at: null,
  content: `<p>${content}</p>`,
  reblog: null,
  account: who,
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
})

async function signedIn(page: import('@playwright/test').Page) {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/verify_credentials**', (r) =>
    r.fulfill({ json: { ...me, source: { privacy: 'public' } } }),
  )
  await page.route('**/api/v1/notifications/unread_count**', (r) =>
    r.fulfill({ json: { count: 0 } }),
  )
  await page.route('**/api/v2/instance', (r) =>
    r.fulfill({ json: {
      domain: 'example.invalid', title: 'Example', configuration: {},
      registrations: { enabled: false, approval_required: false },
    } }),
  )
}

// The server puts direct messages in the home timeline and on profiles.
// Mastodon 5.0 takes them out of both, so the filtering is the client's job —
// and the test has to hand it a timeline that actually contains one.
test('messages are kept out of the home timeline', async ({ page }) => {
  await signedIn(page)
  await page.route('**/api/v1/timelines/home**', (r) =>
    r.fulfill({
      json: [
        status('2', 'a private word', 'direct'),
        status('1', 'a public post', 'public'),
      ],
    }),
  )

  await page.goto('/')
  await expect(page.getByText('a public post')).toBeVisible()
  await expect(page.getByText('a private word')).toHaveCount(0)
})

test('the messages page lists conversations and marks the unread ones', async ({
  page,
}) => {
  await signedIn(page)
  await page.route('**/api/v1/conversations**', (r) =>
    r.fulfill({
      json: [
        { id: '9', unread: true, accounts: [bob], last_status: status('2', 'are you there', 'direct') },
        { id: '8', unread: false, accounts: [bob], last_status: status('1', 'older thread', 'direct') },
      ],
    }),
  )

  await page.goto('/messages')
  await expect(
    page.locator('.column-frame').getByRole('button', { name: 'Messages' }),
  ).toBeVisible()
  await expect(page.getByText('are you there')).toBeVisible()
  await expect(page.getByText('older thread')).toBeVisible()
  // Exactly one of the two is unread.
  await expect(page.getByLabel('Unread')).toHaveCount(1)
  await expect(page.getByText(/not end-to-end encrypted/)).toBeVisible()
})

// The regression that matters: the account's default visibility loads a moment
// after the composer mounts, and used to overwrite `direct` — sending what the
// user believed was a message as a public post.
test('a message is sent as direct, not as the account default', async ({ page }) => {
  await signedIn(page)
  await page.route('**/api/v1/conversations**', (r) => r.fulfill({ json: [] }))

  let posted: Record<string, unknown> | null = null
  await page.route('**/api/v1/statuses', async (route) => {
    posted = route.request().postDataJSON()
    await route.fulfill({ json: status('5', 'hello', 'direct', me) })
  })

  await page.goto('/messages')
  await page.getByRole('button', { name: 'New', exact: true }).click()

  await expect(page.getByRole('heading', { name: 'New message' })).toBeVisible()
  // An unaddressed message has nobody in it yet; "Everyone mentioned" is what
  // the audience menu says once opened.
  await expect(page.getByRole('button', { name: 'Who can see this' })).toContainText(
    'Nobody yet',
  )

  await page.getByRole('textbox').fill('@bob hello')
  await page.getByRole('button', { name: 'Send', exact: true }).click()

  await expect.poll(() => posted).toMatchObject({ visibility: 'direct' })
})

// A message thread reads like a chat window: oldest at the top, newest at the
// bottom, scrolled to the newest on arrival, with the reply box under it.
test('a message thread opens at its newest message', async ({ page }) => {
  await signedIn(page)
  const at = (n: number) => `2026-01-01T00:${String(n).padStart(2, '0')}:00.000Z`
  const thread = Array.from({ length: 30 }, (_, i) => ({
    ...status(String(i + 1), `message ${i + 1}`, 'direct', i % 2 ? me : bob),
    created_at: at(i),
    mentions: [{ id: i % 2 ? bob.id : me.id, username: i % 2 ? 'bob' : 'alice', acct: i % 2 ? 'bob' : 'alice', url: '' }],
  }))
  const newest = thread.at(-1)!
  await page.route('**/api/v1/statuses/30/context', (r) =>
    // Out of order on purpose: the client sorts by time.
    r.fulfill({ json: { ancestors: thread.slice(0, -1).reverse(), descendants: [] } }),
  )
  await page.route('**/api/v1/statuses/30', (r) => r.fulfill({ json: newest }))

  await page.goto('/@alice/30')
  await expect(page.getByRole('heading', { name: 'Bob', exact: true })).toBeVisible()
  await expect(page.getByText('message 30')).toBeInViewport()
  await expect(page.getByText('message 1', { exact: true })).not.toBeInViewport()
  await expect
    .poll(() => page.evaluate(() => window.innerHeight + window.scrollY - document.documentElement.scrollHeight))
    .toBeGreaterThan(-2)
  const order = await page.getByText(/^message \d+$/).allTextContents()
  expect(order.at(0)).toBe('message 1')
  expect(order.at(-1)).toBe('message 30')
  await expect(page.getByRole('button', { name: 'Reply', exact: true }).last()).toBeInViewport()
})
