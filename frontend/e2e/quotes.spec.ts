import { expect, test, type Page } from '@playwright/test'

// A quoted post, as Mastodon's `QuotedStatus` draws it: its content, poll,
// media and link card, but its own quote only named. Every request is stubbed.
const account = (id: string, acct: string) => ({
  id, username: acct.split('@')[0], acct, display_name: acct, note: '',
  url: `https://example.invalid/@${acct}`, uri: `https://example.invalid/users/${acct}`,
  avatar: '', avatar_static: '', header: '', header_static: '', emojis: [], fields: [],
  followers_count: 0, following_count: 0, statuses_count: 1,
  created_at: '2026-01-01T00:00:00Z', locked: false, bot: false, group: false,
})
const alice = account('1', 'alice')
const bob = account('2', 'bob')
const carol = account('3', 'carol')
const status = (id: string, by: ReturnType<typeof account>, extra: Record<string, unknown> = {}) => ({
  id, account: by, content: `<p>Post ${id}</p>`, created_at: '2026-01-01T00:00:00Z',
  sensitive: false, spoiler_text: '', visibility: 'public', language: 'en',
  uri: `https://example.invalid/users/${by.acct}/statuses/${id}`,
  url: `https://example.invalid/@${by.acct}/${id}`, replies_count: 0, reblogs_count: 0,
  favourites_count: 0, edited_at: null, media_attachments: [], mentions: [], tags: [],
  emojis: [], reblog: null, card: null, poll: null, quote: null, ...extra,
})
const image = {
  id: '5', type: 'image', url: '/media/quoted.svg', preview_url: '/media/quoted.svg',
  remote_url: null, description: 'A quoted square', blurhash: null, meta: {},
}
const card = {
  url: 'https://news.example/story', title: 'A quoted story', description: 'What happened',
  type: 'link', author_name: '', author_url: '', provider_name: 'News Example', provider_url: '',
  html: '', width: 400, height: 200, image: null, image_description: '', embed_url: '',
  blurhash: null, published_at: null, authors: [],
}
const poll = {
  id: '7', expires_at: null, expired: true, multiple: false, votes_count: 3, voters_count: 3,
  voted: false, own_votes: [], emojis: [],
  options: [{ title: 'Tea', votes_count: 2 }, { title: 'Coffee', votes_count: 1 }],
}

async function setup(page: Page, quoted: Record<string, unknown>) {
  const requested: string[] = []
  await page.route('**/media/*', (r) => {
    requested.push(new URL(r.request().url()).pathname)
    return r.fulfill({
      contentType: 'image/svg+xml',
      body: '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#287da8" /></svg>',
    })
  })
  const post = status('10', alice, { quote: { state: 'accepted', quoted_status: quoted } })
  await page.route('**/api/v1/statuses/10', (r) => r.fulfill({ json: post }))
  await page.route('**/api/v1/statuses/10/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.goto('/@alice/10')
  return requested
}

test('a quoted post shows its media behind the sensitive cover, unloaded until shown', async ({ page }) => {
  const requested = await setup(page, status('20', bob, { sensitive: true, media_attachments: [image], card }))
  const quote = page.getByTestId('quoted-post')
  await expect(quote.getByText('Post 20')).toBeVisible()
  await expect(quote.getByText(/attachment/)).toHaveCount(0)
  // A card is not shown on a post with media, quoted or not.
  await expect(quote.getByTestId('preview-card')).toHaveCount(0)
  await expect(quote.getByRole('button', { name: 'View image: A quoted square' })).toHaveCount(0)
  expect(requested).not.toContain('/media/quoted.svg')

  // Revealing the media does not open the quoted thread.
  await quote.getByRole('button', { name: /Sensitive content/ }).click()
  await expect(quote.getByRole('button', { name: 'View image: A quoted square' })).toBeVisible()
  await expect(page).toHaveURL(/\/@alice\/10$/)

  // A click elsewhere on the quote does.
  await page.route('**/api/v1/statuses/20', (r) => r.fulfill({ json: status('20', bob) }))
  await page.route('**/api/v1/statuses/20/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await quote.getByText('Post 20').click()
  await expect(page).toHaveURL(/\/@bob\/20$/)
})

test('a quoted post shows its link card and poll', async ({ page }) => {
  await setup(page, status('20', bob, { card, poll }))
  const quote = page.getByTestId('quoted-post')
  await expect(quote.getByTestId('preview-card').getByText('A quoted story')).toBeVisible()
  await expect(quote.getByText('Tea')).toBeVisible()
  await expect(quote.getByText('Coffee')).toBeVisible()
})

test('a quoted post’s own quote is only named, and takes no card', async ({ page }) => {
  await setup(page, status('20', bob, {
    card, quote: { state: 'accepted', quoted_status: status('30', carol, { content: '<p>Deepest</p>' }) },
  }))
  const quote = page.getByTestId('quoted-post')
  await expect(quote.getByText('Quoted a post by @carol')).toBeVisible()
  await expect(page.getByText('Deepest')).toHaveCount(0)
  await expect(quote.getByTestId('preview-card')).toHaveCount(0)
  await expect(page.getByTestId('quoted-post')).toHaveCount(1)
})
