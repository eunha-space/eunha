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

test('a quoted post’s own quote, sent shallow, is fetched to be named', async ({ page }) => {
  await page.route('**/api/v1/statuses/30', (r) => r.fulfill({ json: status('30', carol) }))
  await setup(page, status('20', bob, { quote: { state: 'accepted', quoted_status_id: '30' } }))
  const quote = page.getByTestId('quoted-post')
  await expect(quote.getByText('Quoted a post by @carol')).toBeVisible()
  await expect(quote.getByText('Post unavailable')).toHaveCount(0)
})

test('a quoted post’s own quote that cannot be fetched is unavailable', async ({ page }) => {
  await page.route('**/api/v1/statuses/30', (r) => r.fulfill({ status: 404, json: { error: 'Record not found' } }))
  await setup(page, status('20', bob, { quote: { state: 'accepted', quoted_status_id: '30' } }))
  await expect(page.getByTestId('quoted-post').getByText('Post unavailable')).toBeVisible()
})

async function signedIn(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
    localStorage.setItem('eunha:me-id', '1')
  })
  await page.route('**/api/v1/accounts/verify_credentials**', (r) =>
    r.fulfill({ json: { ...alice, source: { privacy: 'public' } } }))
  await page.route('**/api/v1/notifications/unread_count**', (r) => r.fulfill({ json: { count: 0 } }))
  await page.route('**/api/v1/markers', (r) => r.fulfill({ json: {} }))
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [] }))
}

test('the quoted author can remove their post from a quote, after confirming', async ({ page }) => {
  await signedIn(page)
  const quoting = status('10', bob, { quote: { state: 'accepted', quoted_status: status('20', alice) } })
  await page.route('**/api/v1/statuses/10', (r) => r.fulfill({ json: quoting }))
  await page.route('**/api/v1/statuses/10/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  let revoked = false
  await page.route('**/api/v1/statuses/20/quotes/10/revoke', (r) => {
    revoked = r.request().method() === 'POST'
    return r.fulfill({ json: { ...quoting, quote: { state: 'revoked', quoted_status: null } } })
  })
  await page.goto('/@bob/10')
  await page.getByRole('button', { name: 'More' }).click()
  await page.getByRole('menuitem', { name: 'Remove my post from @bob’s post' }).click()
  await expect(page.getByText('This action cannot be undone.')).toBeVisible()
  await page.getByRole('button', { name: 'Remove post' }).click()
  await expect(page.getByText('Post removed by author')).toBeVisible()
  expect(revoked).toBe(true)
})

test('a quote of your post is not offered for removal to its own author', async ({ page }) => {
  await signedIn(page)
  const quoting = status('10', alice, { quote: { state: 'accepted', quoted_status: status('20', alice) } })
  await page.route('**/api/v1/statuses/10', (r) => r.fulfill({ json: quoting }))
  await page.route('**/api/v1/statuses/10/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.goto('/@alice/10')
  await page.getByRole('button', { name: 'More' }).click()
  await expect(page.getByRole('menuitem', { name: /Remove my post/ })).toHaveCount(0)
})

test('a quote notification says once where to remove the quote', async ({ page }) => {
  await signedIn(page)
  const quoting = (id: string) => status(id, bob, { quote: { state: 'accepted', quoted_status: status('20', alice) } })
  await page.route('**/api/v1/notifications**', (r) => r.fulfill({ json: [
    { id: '1', type: 'quote', created_at: '2026-01-01T00:00:00Z', account: bob, status: quoting('10') },
    { id: '2', type: 'quote', created_at: '2026-01-01T00:00:00Z', account: bob, status: quoting('11') },
  ] }))
  await page.route('**/api/v1/notifications/unread_count**', (r) => r.fulfill({ json: { count: 0 } }))
  await page.goto('/notifications')
  await expect(page.getByText('Want to remove your quoted post?')).toHaveCount(1)
  await page.getByRole('button', { name: 'Got it' }).click()
  await expect(page.getByText('Want to remove your quoted post?')).toHaveCount(0)
  await page.reload()
  await expect(page.getByText('quoted your post').first()).toBeVisible()
  await expect(page.getByText('Want to remove your quoted post?')).toHaveCount(0)
})

test('quoting a post opens the composer with the post in it', async ({ page }) => {
  await signedIn(page)
  const post = status('10', bob, {
    card: { ...card, authors: [{ name: 'Bob', url: '', account: bob }] },
    quote_approval: { automatic: ['public'], manual: [], current_user: 'automatic' },
  })
  await page.route('**/api/v1/statuses/10', (r) => r.fulfill({ json: post }))
  await page.route('**/api/v1/statuses/10/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.goto('/@bob/10')
  await page.getByRole('button', { name: 'More' }).click()
  await page.getByRole('menuitem', { name: 'Quote' }).click()
  await expect(page.getByRole('heading', { name: 'Quote post' })).toBeVisible()
  await expect(page.getByText('Quoting this post')).toBeVisible()
})

test('opening a post’s menu dismisses the remove-quote hint', async ({ page }) => {
  await signedIn(page)
  const quoting = status('10', bob, { quote: { state: 'accepted', quoted_status: status('20', alice) } })
  await page.route('**/api/v1/notifications**', (r) => r.fulfill({ json: [
    { id: '1', type: 'quote', created_at: '2026-01-01T00:00:00Z', account: bob, status: quoting },
  ] }))
  await page.route('**/api/v1/notifications/unread_count**', (r) => r.fulfill({ json: { count: 0 } }))
  await page.goto('/notifications')
  await expect(page.getByText('Want to remove your quoted post?')).toBeVisible()
  await page.getByRole('button', { name: 'More', exact: true }).first().click()
  await expect(page.getByText('Want to remove your quoted post?')).toHaveCount(0)
})

const approval = (current_user: string, automatic = ['public'], manual: string[] = []) =>
  ({ automatic, manual, current_user })

async function openPost(page: Page, post: Record<string, unknown>) {
  await page.route(`**/api/v1/statuses/${post.id}`, (r) => r.fulfill({ json: post }))
  await page.route(`**/api/v1/statuses/${post.id}/context`, (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.goto(`/@${(post.account as { acct: string }).acct}/${post.id}`)
  await page.getByRole('button', { name: 'More' }).click()
}

test('a post the reader may not quote says why, and cannot be chosen', async ({ page }) => {
  await signedIn(page)
  await openPost(page, status('10', bob, { quote_approval: approval('denied', []) }))
  const item = page.getByRole('menuitem', { name: /Quote/ })
  await expect(item).toContainText('You are not allowed to quote this post')
  await expect(item).toHaveAttribute('aria-disabled', 'true')
})

test('a post only followers may quote says so', async ({ page }) => {
  await signedIn(page)
  await openPost(page, status('10', bob, { quote_approval: approval('denied', ['followers']) }))
  await expect(page.getByRole('menuitem', { name: /Quote/ })).toContainText('Only followers can quote this post')
})

test('someone else’s followers-only post cannot be quoted', async ({ page }) => {
  await signedIn(page)
  await openPost(page, status('10', bob, { visibility: 'private', quote_approval: approval('denied', []) }))
  const item = page.getByRole('menuitem', { name: /Quote/ })
  await expect(item).toContainText('Private posts cannot be quoted')
  await expect(item).toHaveAttribute('aria-disabled', 'true')
})

test('a post without a quote policy for the reader fetches one when the menu opens', async ({ page }) => {
  await signedIn(page)
  let fetches = 0
  await page.route('**/api/v1/statuses/10', (r) => {
    fetches += 1
    return r.fulfill({ json: status('10', bob, fetches === 1 ? { quote_approval: null } : { quote_approval: approval('automatic') }) })
  })
  await page.route('**/api/v1/statuses/10/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.goto('/@bob/10')
  await page.getByRole('button', { name: 'More' }).click()
  await expect(page.getByRole('menuitem', { name: 'Quote' })).not.toHaveAttribute('aria-disabled', 'true')
  expect(fetches).toBe(2)
})

test('your own followers-only post can be quoted, and the quote stays with your followers', async ({ page }) => {
  await signedIn(page)
  await page.route('**/api/eunha/v1/preferences', (r) => r.fulfill({ json: { default_spoiler_field: false } }))
  await openPost(page, status('10', alice, { visibility: 'private', quote_approval: approval('automatic', []) }))
  await page.getByRole('menuitem', { name: 'Quote' }).click()
  await expect(page.getByRole('heading', { name: 'Quote post' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Who can see this' })).toContainText('Your followers')
  let posted: Record<string, unknown> | null = null
  await page.route('**/api/v1/statuses', async (r) => {
    posted = r.request().postDataJSON()
    await r.fulfill({ json: status('11', alice, { visibility: 'private' }) })
  })
  await page.getByRole('button', { name: 'Quote', exact: true }).click()
  await expect.poll(() => posted).toMatchObject({ visibility: 'private', quoted_status_id: '10' })
})

test('your own followers-only post can be boosted to your followers again', async ({ page }) => {
  await signedIn(page)
  await page.route('**/api/v1/statuses/10', (r) => r.fulfill({ json: status('10', alice, { visibility: 'private' }) }))
  await page.route('**/api/v1/statuses/10/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  let boosted = false
  await page.route('**/api/v1/statuses/10/reblog', (r) => {
    boosted = true
    return r.fulfill({ json: status('12', alice, { reblog: status('10', alice, { visibility: 'private', reblogged: true, reblogs_count: 1 }) }) })
  })
  await page.goto('/@alice/10')
  const boost = page.getByRole('button', { name: 'Boost' })
  await expect(boost).toHaveAttribute('title', 'Share again with your followers')
  await boost.click()
  await expect.poll(() => boosted).toBe(true)
})

test('someone else’s followers-only post cannot be boosted', async ({ page }) => {
  await signedIn(page)
  await page.route('**/api/v1/statuses/10', (r) => r.fulfill({ json: status('10', bob, { visibility: 'private' }) }))
  await page.route('**/api/v1/statuses/10/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.goto('/@bob/10')
  const boost = page.getByRole('button', { name: 'Boost' })
  await expect(boost).toBeDisabled()
  await expect(boost).toHaveAttribute('title', 'This post cannot be boosted')
})
