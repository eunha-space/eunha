import { expect, test, type Page } from '@playwright/test'

// Custom emoji are drawn as Mastodon's web client draws them: a `:shortcode:`
// becomes an image only when the entity lists it in its `emojis`, the image is
// the still file until hovered (or always the animated one when GIFs
// auto-play), and nothing in a shortcode or an emoji's URL becomes markup.
// Every request is stubbed.
const blob = { shortcode: 'blob', url: '/emoji/blob.gif', static_url: '/emoji/blob.png', visible_in_picker: true }
const account = {
  id: '1', username: 'alice', acct: 'alice', display_name: 'Alice :blob:', note: '<p>Bio :blob:</p>',
  url: 'https://example.invalid/@alice', uri: 'https://example.invalid/users/alice',
  avatar: '/media/avatar.gif', avatar_static: '/media/avatar.png',
  header: '', header_static: '', emojis: [blob],
  fields: [{ name: 'Mood :blob:', value: '<p>Fine :blob:</p>', verified_at: null }],
  followers_count: 0, following_count: 0, statuses_count: 1,
  created_at: '2026-01-01T00:00:00Z', locked: false, bot: false, group: false,
}
const post = {
  id: '10', account, created_at: '2026-01-01T00:00:00Z',
  content:
    '<p>Hello :blob: and :nope: <a href="https://example.invalid/:blob:" title=":blob:">link :blob:</a></p>',
  sensitive: false, spoiler_text: '', visibility: 'public', language: 'en',
  uri: 'https://example.invalid/users/alice/statuses/10', url: 'https://example.invalid/@alice/10',
  replies_count: 0, reblogs_count: 0, favourites_count: 0, edited_at: null,
  media_attachments: [], mentions: [], tags: [], emojis: [blob], reblog: null, card: null,
  poll: {
    id: '5', expires_at: null, expired: true, multiple: false, votes_count: 1, voters_count: 1,
    options: [{ title: 'Yes :blob:', votes_count: 1 }, { title: 'No :blob:', votes_count: 0 }],
    emojis: [blob], voted: false, own_votes: [],
  },
}

async function stubMedia(page: Page) {
  const requested: string[] = []
  await page.route(/\/(emoji|media)\//, (r) => {
    requested.push(new URL(r.request().url()).pathname)
    return r.fulfill({
      contentType: 'image/svg+xml',
      body: '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#c44" /></svg>',
    })
  })
  return requested
}

async function signIn(page: Page, autoPlay: boolean) {
  await page.addInitScript((autoPlayGif) => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
    // As a page saved by an earlier visit, so the first paint has it.
    localStorage.setItem('eunha:reading-preferences:1', JSON.stringify({
      displayMedia: 'default', expandSpoilers: false, autoPlayGif,
    }))
  }, autoPlay)
  await page.route('**/api/v1/accounts/verify_credentials**', (r) => r.fulfill({ json: {
    ...account, source: { privacy: 'public' }, role: { id: '1', name: '', permissions: '0' },
  } }))
  await page.route('**/api/eunha/v1/preferences', (r) => r.fulfill({ json: {
    noindex: false, show_application: true, chosen_languages: null, locale: null, time_zone: null,
    always_send_emails: false, aggregate_reblogs: true, notification_emails: {},
    display_media: 'default', expand_content_warnings: false, auto_play: autoPlay,
  } }))
  await page.route('**/api/v1/notifications/unread_count**', (r) => r.fulfill({ json: { count: 0 } }))
  await page.route('**/api/eunha/v1/terms_of_service/interstitial', (r) => r.fulfill({ status: 204 }))
}

async function openPost(page: Page, changes: Record<string, unknown> = {}) {
  await page.route('**/api/v1/statuses/10', (r) => r.fulfill({ json: { ...post, ...changes } }))
  await page.route('**/api/v1/statuses/10/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.goto('/@alice/10')
}

test('a post draws its listed emoji, still until hovered, and leaves the rest as text', async ({ page }) => {
  const requested = await stubMedia(page)
  await openPost(page)

  const content = page.locator('.status-content')
  await expect(content).toContainText('Hello')
  const emoji = content.locator('img.custom-emoji').first()
  await expect(emoji).toHaveAttribute('alt', ':blob:')
  await expect(emoji).toHaveAttribute('title', ':blob:')
  await expect(emoji).toHaveAttribute('src', /\/emoji\/blob\.png$/)
  // One in the text and one in the link's text; none from the attributes.
  await expect(content.locator('img.custom-emoji')).toHaveCount(2)
  await expect(content.locator('a')).toHaveAttribute('href', 'https://example.invalid/:blob:')
  await expect(content.locator('a')).toHaveAttribute('title', ':blob:')
  // Not one of the post's emoji, so it stays as typed.
  await expect(content).toContainText(':nope:')
  expect(requested).not.toContain('/emoji/blob.gif')

  await content.hover()
  await expect(emoji).toHaveAttribute('src', /\/emoji\/blob\.gif$/)
  await page.mouse.move(0, 0)
  await expect(emoji).toHaveAttribute('src', /\/emoji\/blob\.png$/)

  // The display name and the poll's options draw theirs too.
  const name = page.getByRole('link', { name: /Alice/ }).filter({ has: page.locator('img.custom-emoji') })
  await expect(name.locator('img.custom-emoji')).toHaveAttribute('src', /\/emoji\/blob\.png$/)
  await name.hover()
  await expect(name.locator('img.custom-emoji')).toHaveAttribute('src', /\/emoji\/blob\.gif$/)
  await expect(page.getByText('Yes').locator('img.custom-emoji')).toHaveAttribute('alt', ':blob:')
})

test('a content warning draws the post’s emoji', async ({ page }) => {
  await stubMedia(page)
  await openPost(page, { spoiler_text: 'Spoilers :blob: :nope:', sensitive: true })
  const warning = page.getByText('Spoilers')
  await expect(warning.locator('img.custom-emoji')).toHaveAttribute('alt', ':blob:')
  await expect(warning).toContainText(':nope:')
})

test('auto-playing GIFs draws the animated files without a hover', async ({ page }) => {
  const requested = await stubMedia(page)
  await signIn(page, true)
  await openPost(page)
  const emoji = page.locator('.status-content img.custom-emoji').first()
  await expect(emoji).toHaveAttribute('src', /\/emoji\/blob\.gif$/)
  expect(requested).not.toContain('/emoji/blob.png')
})

test('a shortcode or URL that is not an emoji’s stays inert', async ({ page }) => {
  await stubMedia(page)
  await page.addInitScript(() => {
    ;(window as unknown as { __xss?: number }).__xss = 0
  })
  await openPost(page, {
    content: '<p>:evil: :quote: :scheme:</p>',
    emojis: [
      // A shortcode Mastodon could never have, carrying markup.
      { shortcode: 'x"><img src=x onerror="window.__xss=1">', url: '/emoji/x.png', static_url: '/emoji/x.png' },
      // A URL that tries to close the attribute it lands in.
      { shortcode: 'quote', url: '/emoji/q.png" onerror="window.__xss=1', static_url: '/emoji/q.png" onerror="window.__xss=1' },
      // A URL that is not on the web.
      { shortcode: 'scheme', url: 'javascript:window.__xss=1', static_url: 'javascript:window.__xss=1' },
    ],
    account: {
      ...account,
      display_name: '<img src=x onerror="window.__xss=1"> :blob:',
    },
  })
  const content = page.locator('.status-content')
  await expect(content).toContainText(':evil:')
  await expect(content).toContainText(':scheme:')
  const quote = content.locator('img.custom-emoji')
  await expect(quote).toHaveCount(1)
  await expect(quote).not.toHaveAttribute('onerror')
  expect(await quote.getAttribute('src')).toContain('%22%20onerror=%22')
  // The display name is text, markup and all.
  await expect(page.getByText('<img src=x onerror="window.__xss=1">').first()).toBeVisible()
  expect(await page.evaluate(() => (window as unknown as { __xss?: number }).__xss)).toBe(0)
})

test('a profile draws its emoji in the name, bio and fields, animating together on hover', async ({ page }) => {
  await stubMedia(page)
  await page.route('**/api/v1/accounts/lookup**', (r) => r.fulfill({ json: account }))
  await page.goto('/@alice')
  const bio = page.getByText('Bio')
  await expect(bio.locator('img.custom-emoji')).toHaveAttribute('src', /\/emoji\/blob\.png$/)
  const field = page.locator('dt', { hasText: 'Mood' })
  await expect(field.locator('img.custom-emoji')).toHaveAttribute('alt', ':blob:')
  await expect(page.locator('dd', { hasText: 'Fine' }).locator('img.custom-emoji')).toHaveAttribute('alt', ':blob:')
  // Hovering the bio animates the name's emoji too: the header is one element.
  await bio.hover()
  await expect(page.locator('.text-xl img.custom-emoji')).toHaveAttribute('src', /\/emoji\/blob\.gif$/)
  await expect(field.locator('img.custom-emoji')).toHaveAttribute('src', /\/emoji\/blob\.gif$/)
})

test('the composer suggests custom emoji on a colon and inserts the picked one', async ({ page }) => {
  await stubMedia(page)
  await signIn(page, false)
  await page.route('**/api/v1/custom_emojis', (r) => r.fulfill({ json: [
    blob,
    { shortcode: 'blobcat', url: '/emoji/blobcat.gif', static_url: '/emoji/blobcat.png', visible_in_picker: true, category: 'Cats' },
    { shortcode: 'hidden_blob', url: '/emoji/h.png', static_url: '/emoji/h.png', visible_in_picker: false },
  ] }))
  await page.route('**/api/v1/timelines/home**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/announcements**', (r) => r.fulfill({ json: [] }))
  await page.goto('/')
  await page.getByRole('button', { name: 'New post', exact: true }).click()
  const field = page.getByPlaceholder('What would you like to say?')
  await field.click()
  await field.pressSequentially('Hi :b')
  // One letter is not enough to ask.
  await expect(page.getByRole('listbox', { name: 'Emoji suggestions' })).toHaveCount(0)
  await field.pressSequentially('lob')
  const list = page.getByRole('listbox', { name: 'Emoji suggestions' })
  await expect(list.getByRole('option')).toHaveCount(2)
  await expect(list.getByRole('option').first()).toContainText(':blob:')
  await field.press('ArrowDown')
  await expect(list.getByRole('option', { selected: true })).toContainText(':blobcat:')
  await field.press('Enter')
  await expect(field).toHaveValue('Hi :blobcat: ')

  // The picker lists the server's emoji by category and inserts at the caret.
  await page.getByRole('button', { name: 'Insert emoji' }).click()
  await expect(page.getByRole('region', { name: 'Cats' })).toBeVisible()
  await expect(page.getByRole('button', { name: ':hidden_blob:' })).toHaveCount(0)
  await page.getByRole('button', { name: ':blob:', exact: true }).click()
  await expect(field).toHaveValue('Hi :blobcat: :blob: ')
})

test('the composer suggests Unicode emoji too, and inserts the emoji itself', async ({ page }) => {
  await stubMedia(page)
  await signIn(page, false)
  await page.route('**/api/v1/custom_emojis', (r) => r.fulfill({ json: [
    blob,
    { shortcode: 'thumbsup_parrot', url: '/emoji/p.gif', static_url: '/emoji/p.png', visible_in_picker: true },
  ] }))
  await page.route('**/api/v1/timelines/home**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/announcements**', (r) => r.fulfill({ json: [] }))
  await page.goto('/')
  await page.getByRole('button', { name: 'New post', exact: true }).click()
  const field = page.getByPlaceholder('What would you like to say?')
  await field.click()
  await field.pressSequentially('Yes :thumbs_up')
  const list = page.getByRole('listbox', { name: 'Emoji suggestions' })
  // Custom and Unicode emoji are offered together, ranked as Mastodon's
  // search ranks them; a Unicode one is named by its label in snake case.
  await expect(list.getByRole('option')).toContainText([':thumbsup_parrot:', ':thumbs_up:'])
  const thumbs = list.getByRole('option').filter({ hasText: ':thumbs_up:' })
  await expect(thumbs).toContainText('👍')
  await thumbs.getByRole('button').dispatchEvent('mousedown')
  await expect(field).toHaveValue('Yes 👍 ')

  await field.pressSequentially(':party_pop')
  await expect(list.getByRole('option').first()).toContainText('🎉')
  await field.press('Enter')
  await expect(field).toHaveValue('Yes 👍 🎉 ')
})

test('the picker offers Unicode emoji by category, searches them, and remembers what is used', async ({ page }) => {
  await stubMedia(page)
  await signIn(page, false)
  await page.route('**/api/v1/custom_emojis', (r) => r.fulfill({ json: [blob] }))
  await page.route('**/api/v1/timelines/home**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/announcements**', (r) => r.fulfill({ json: [] }))
  await page.goto('/')
  await page.getByRole('button', { name: 'New post', exact: true }).click()
  const field = page.getByPlaceholder('What would you like to say?')

  await page.getByRole('button', { name: 'Insert emoji' }).click()
  // Mastodon's categories, in its order, after the custom ones.
  const nav = page.getByRole('navigation', { name: 'Emoji categories' })
  await expect.poll(() =>
    nav.getByRole('button').evaluateAll((els) => els.map((e) => e.getAttribute('aria-label'))),
  ).toEqual([
    'Frequently used', 'Custom', 'People', 'Nature', 'Food & Drink', 'Activity',
    'Travel & Places', 'Objects', 'Symbols', 'Flags',
  ])
  // Mastodon's defaults fill the frequently used row until there is a history.
  const frequent = page.getByRole('region', { name: 'Frequently used' })
  await expect(frequent.getByRole('button').first()).toHaveAttribute('aria-label', /^👍 /)
  await expect(page.getByRole('region', { name: 'Food & Drink' }).getByRole('button', { name: /^🍕 / })).toBeAttached()

  await page.getByRole('searchbox', { name: 'Search emoji' }).fill('tada')
  const results = page.getByRole('region', { name: 'Search results' })
  await results.getByRole('button', { name: /^🎉 / }).click()
  await expect(field).toHaveValue('🎉 ')

  await page.getByRole('button', { name: 'Insert emoji' }).click()
  await page.getByRole('searchbox', { name: 'Search emoji' }).fill('zzzzqq')
  await expect(page.getByText('No matching emojis found')).toBeVisible()
  await page.getByRole('searchbox', { name: 'Search emoji' }).fill('')
  // What was picked leads the row now.
  await expect(frequent.getByRole('button').first()).toHaveAttribute('aria-label', /^🎉 /)
  await frequent.getByRole('button', { name: /^🎉 / }).click()
  await expect(field).toHaveValue('🎉 🎉 ')
})
