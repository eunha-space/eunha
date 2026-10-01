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

const status = (id: string, content: string, filtered: unknown[]) => ({
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
  pinned: false,
  filtered,
})

const result = (title: string, action: string, context: string[]) => ({
  filter: { id: title, title, context, expires_at: null, filter_action: action },
  keyword_matches: [title],
  status_matches: null,
})

// The server marks a post with every filter it matches and leaves acting on
// them to the client, as Mastodon does: on the profile ("account" context) a
// hide filter drops the post, a warning folds it, and a filter for another
// context does nothing.
test('filters hide or fold posts in their own context', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:token', 'test-token')
  })
  await page.route('**/api/v1/accounts/lookup**', (route) => route.fulfill({ json: account }))
  await page.route('**/api/v1/accounts/verify_credentials**', (route) =>
    route.fulfill({ json: account }),
  )
  await page.route('**/api/v1/accounts/relationships**', (route) => route.fulfill({ json: [] }))
  await page.route('**/api/v1/accounts/1/statuses**', (route) =>
    route.fulfill({
      json: [
        status('1', 'hidden here', [result('nope', 'hide', ['account'])]),
        status('2', 'folded here', [result('spoilers', 'warn', ['account'])]),
        status('3', 'shown here', [result('elsewhere', 'hide', ['home'])]),
      ],
    }),
  )
  await page.route('**/api/v1/accounts/1/statuses?pinned=true**', (route) =>
    route.fulfill({ json: [] }),
  )

  await page.goto('/@alice')
  await expect(page.getByText('shown here')).toBeVisible()
  await expect(page.getByText('hidden here')).toHaveCount(0)
  await expect(page.getByText('Filtered: spoilers')).toBeVisible()
  await expect(page.getByText('folded here')).toHaveCount(0)
  await page.getByRole('button', { name: 'Show anyway' }).click()
  await expect(page.getByText('folded here')).toBeVisible()
})
