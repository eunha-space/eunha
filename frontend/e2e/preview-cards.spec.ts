import { expect, test, type Page } from '@playwright/test'

// A post's link preview, as Mastodon's web client shows `status.card`. Every
// request is stubbed.
const account = {
  id: '1', username: 'alice', acct: 'alice', display_name: 'Alice', note: '',
  url: 'https://example.invalid/@alice', uri: 'https://example.invalid/users/alice',
  avatar: '/media/avatar.gif', avatar_static: '/media/avatar.png',
  header: '', header_static: '', emojis: [], fields: [],
  followers_count: 0, following_count: 0, statuses_count: 1,
  created_at: '2026-01-01T00:00:00Z', locked: false, bot: false, group: false,
}
const bob = { ...account, id: '2', username: 'bob', acct: 'bob@news.example', display_name: 'Bob Writer',
  avatar: '/media/bob.gif', avatar_static: '/media/bob.png' }
const BLURHASH = 'LEHV6nWB2yk8pyo0adR*.7kCMdnj'
const linkCard = {
  url: 'https://news.example/story', title: 'A story worth reading', description: 'What happened next',
  type: 'link', author_name: '', author_url: '', provider_name: 'News Example', provider_url: '',
  html: '', width: 400, height: 200, image: '/cards/story.png', image_description: 'A harbour',
  embed_url: '', blurhash: BLURHASH, published_at: null, authors: [],
}
const post = {
  id: '10', account, content: '<p>Look <a href="https://news.example/story">here</a></p>',
  created_at: '2026-01-01T00:00:00Z', sensitive: false, spoiler_text: '', visibility: 'public',
  language: 'en', uri: 'https://example.invalid/users/alice/statuses/10',
  url: 'https://example.invalid/@alice/10', replies_count: 0, reblogs_count: 0, favourites_count: 0,
  edited_at: null, media_attachments: [], mentions: [], tags: [], emojis: [], reblog: null,
  card: linkCard, poll: null,
}

type Display = 'default' | 'show_all' | 'hide_all'

async function setup(page: Page, changes: Record<string, unknown>, displayMedia: Display | null = null) {
  if (displayMedia) {
    await page.addInitScript((display) => {
      localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
      localStorage.setItem('eunha:active-account', '1')
      localStorage.setItem('eunha:reading-preferences:1', JSON.stringify({
        displayMedia: display, expandSpoilers: false, autoPlayGif: false,
      }))
    }, displayMedia)
    await page.route('**/api/v1/accounts/verify_credentials**', (r) => r.fulfill({ json: {
      ...account, source: { privacy: 'public' }, role: { id: '1', name: '', permissions: '0' },
    } }))
    await page.route('**/api/eunha/v1/preferences', (r) => r.fulfill({ json: {
      noindex: false, show_application: true, chosen_languages: null, locale: null, time_zone: null,
      always_send_emails: false, aggregate_reblogs: true, notification_emails: {},
      display_media: displayMedia, expand_content_warnings: false, auto_play: false,
    } }))
    await page.route('**/api/v1/notifications/unread_count**', (r) => r.fulfill({ json: { count: 0 } }))
    await page.route('**/api/eunha/v1/terms_of_service/interstitial', (r) => r.fulfill({ status: 204 }))
  }
  const requested: string[] = []
  await page.route(/\/(cards|media)\//, (r) => {
    requested.push(new URL(r.request().url()).pathname)
    return r.fulfill({
      contentType: 'image/svg+xml',
      body: '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#287da8" /></svg>',
    })
  })
  await page.route('**/api/v1/statuses/10', (r) => r.fulfill({ json: { ...post, ...changes } }))
  await page.route('**/api/v1/statuses/10/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.goto('/@alice/10')
  return requested
}

const card = (page: Page) => page.getByTestId('preview-card')

test('a link card shows its image, provider, title and description', async ({ page }) => {
  await setup(page, {})
  await expect(card(page)).toBeVisible()
  await expect(card(page).getByText('News Example')).toBeVisible()
  const title = card(page).getByRole('link', { name: /A story worth reading/ })
  await expect(title).toHaveAttribute('href', 'https://news.example/story')
  await expect(title).toHaveAttribute('target', '_blank')
  await expect(card(page).getByText('What happened next')).toBeVisible()
  const image = card(page).locator('img[src="/cards/story.png"]')
  await expect(image).toHaveAttribute('alt', 'A harbour')
})

test('a card names its author by name, or links them when they have an account', async ({ page }) => {
  await setup(page, { card: { ...linkCard, author_name: 'Carol Reporter' } })
  await expect(card(page).getByText('Carol Reporter')).toBeVisible()
  // Mastodon shows the author in place of the description.
  await expect(card(page).getByText('What happened next')).toHaveCount(0)
})

test('an author with an account is offered as more from them, with a still avatar', async ({ page }) => {
  await setup(page, { card: { ...linkCard, authors: [{ name: 'Bob', url: 'https://news.example/bob', account: bob }] } })
  const more = page.getByText('More from')
  await expect(more).toBeVisible()
  const author = page.getByRole('link', { name: 'Bob Writer' })
  await expect(author).toHaveAttribute('href', '/@bob@news.example')
  await expect(author.locator('img')).toHaveAttribute('src', '/media/bob.png')
  await author.hover()
  await expect(author.locator('img')).toHaveAttribute('src', '/media/bob.gif')
})

test('a card without an image draws an icon and its provider from the host', async ({ page }) => {
  await setup(page, { card: { ...linkCard, image: null, provider_name: '', url: 'https://xn--bcher-kva.example/page' } })
  await expect(card(page).getByText('bücher.example')).toBeVisible()
  await expect(card(page).locator('img')).toHaveCount(0)
})

test('a video card loads its player only on a click, sandboxed', async ({ page }) => {
  await setup(page, { card: {
    ...linkCard, type: 'video', provider_name: 'Tube', url: 'https://tube.example/watch/1',
    html: '<iframe src="https://tube.example/embed/1?a=1" width="480" height="270" onload="window.__x=1"></iframe><script>window.__x=1</script>',
  } })
  await expect(page.locator('iframe')).toHaveCount(0)
  await card(page).getByRole('button', { name: 'Play' }).click()
  const frame = page.locator('iframe')
  await expect(frame).toHaveAttribute('src', 'https://tube.example/embed/1?a=1&autoplay=1&auto_play=1')
  await expect(frame).toHaveAttribute('sandbox', 'allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-forms')
  await expect(frame).not.toHaveAttribute('onload')
  expect(await page.evaluate(() => (window as unknown as { __x?: number }).__x)).toBeUndefined()
})

test('a sensitive post’s card is hidden behind its blurhash, and its image not loaded until shown', async ({ page }) => {
  const requested = await setup(page, { sensitive: true })
  await expect(card(page).getByRole('button', { name: /Sensitive content/ })).toBeVisible()
  await expect(card(page).locator('canvas')).toHaveCount(1)
  await expect(card(page).locator('img')).toHaveCount(0)
  expect(requested).not.toContain('/cards/story.png')
  await card(page).getByRole('button', { name: /Sensitive content/ }).click()
  await expect(card(page).locator('img[src="/cards/story.png"]')).toBeVisible()
  expect(requested).toContain('/cards/story.png')
})

test('showing all media does not uncover a sensitive post’s card', async ({ page }) => {
  const requested = await setup(page, { sensitive: true }, 'show_all')
  await expect(card(page).getByRole('button', { name: /Sensitive content/ })).toBeVisible()
  expect(requested).not.toContain('/cards/story.png')
})

test('hiding all media hides every card’s image', async ({ page }) => {
  const requested = await setup(page, {}, 'hide_all')
  await expect(card(page).getByRole('button', { name: /Sensitive content/ })).toBeVisible()
  await expect(card(page).locator('img')).toHaveCount(0)
  expect(requested).not.toContain('/cards/story.png')
})

test('a hidden video card’s description uncovers it rather than leaving', async ({ page }) => {
  await setup(page, { sensitive: true, card: { ...linkCard, type: 'video', html: '<iframe src="https://tube.example/embed/1"></iframe>' } })
  await card(page).getByRole('link', { name: /A story worth reading/ }).click()
  await expect(card(page).getByRole('button', { name: 'Play' })).toBeVisible()
  await expect(page).toHaveURL(/\/@alice\/10$/)
})

test('no card is shown on a post with media or a quote', async ({ page }) => {
  await setup(page, { media_attachments: [{
    id: '1', type: 'image', url: '/media/1.svg', preview_url: '/media/1.svg', remote_url: null,
    description: 'A blue square', blurhash: null, meta: {},
  }] })
  await expect(page.getByRole('button', { name: 'View image: A blue square' })).toBeVisible()
  await expect(card(page)).toHaveCount(0)
})

test('no card is shown on a post that quotes another', async ({ page }) => {
  await setup(page, { quote: { state: 'pending', quoted_status: null } })
  await expect(page.getByText('Post pending')).toBeVisible()
  await expect(card(page)).toHaveCount(0)
})
