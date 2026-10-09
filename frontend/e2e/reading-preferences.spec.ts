import { expect, test, type Page } from '@playwright/test'

// The account's reading preferences (`web.display_media`,
// `web.expand_content_warnings`, `web.auto_play`) decide how posts are shown,
// as they do in Mastodon's web client. Every request is stubbed.
const account = {
  id: '1', username: 'alice', acct: 'alice', display_name: 'Alice', note: '',
  url: 'https://example.invalid/@alice', uri: 'https://example.invalid/users/alice',
  avatar: '/media/avatar.gif', avatar_static: '/media/avatar.png',
  header: '', header_static: '', emojis: [], fields: [],
  followers_count: 0, following_count: 0, statuses_count: 1,
  created_at: '2026-01-01T00:00:00Z', locked: false, bot: false, group: false,
}
const image = {
  id: '1', type: 'image', url: '/media/1.svg', preview_url: '/media/1.svg',
  remote_url: null, description: 'A blue square', blurhash: null, meta: {},
}
const gifv = {
  id: '2', type: 'gifv', url: '/media/2.mp4', preview_url: '/media/1.svg',
  remote_url: null, description: 'A spinning square', blurhash: null, meta: {},
}
const post = {
  id: '10', account, content: '<p>What happens next</p>', created_at: '2026-01-01T00:00:00Z',
  sensitive: true, spoiler_text: 'Film spoilers', visibility: 'public', language: 'en',
  uri: 'https://example.invalid/users/alice/statuses/10', url: 'https://example.invalid/@alice/10',
  replies_count: 0, reblogs_count: 0, favourites_count: 0, edited_at: null,
  media_attachments: [image, gifv], mentions: [], tags: [], emojis: [], reblog: null,
  card: null, poll: null,
}

interface Reading {
  display_media: 'default' | 'show_all' | 'hide_all'
  expand_content_warnings: boolean
  auto_play: boolean
}

async function setup(page: Page, reading: Reading | null, changes: Partial<typeof post> = {}) {
  if (reading) {
    await page.addInitScript(() => {
      localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
      localStorage.setItem('eunha:active-account', '1')
    })
    await page.route('**/api/v1/accounts/verify_credentials**', r => r.fulfill({ json: {
      ...account, source: { privacy: 'public' }, role: { id: '1', name: '', permissions: '0' },
    } }))
    await page.route('**/api/eunha/v1/preferences', r => r.fulfill({ json: {
      noindex: false, show_application: true, chosen_languages: null, locale: null,
      time_zone: null, always_send_emails: false, aggregate_reblogs: true,
      notification_emails: {}, ...reading,
    } }))
    await page.route('**/api/v1/notifications/unread_count**', r => r.fulfill({ json: { count: 0 } }))
    await page.route('**/api/eunha/v1/terms_of_service/interstitial', r => r.fulfill({ status: 204 }))
  }
  await page.route('**/api/v1/statuses/10', r => r.fulfill({ json: { ...post, ...changes } }))
  await page.route('**/api/v1/statuses/10/context', r => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.route('**/media/*', r => r.fulfill({
    contentType: 'image/svg+xml',
    body: '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#287da8" /></svg>',
  }))
  await page.goto('/@alice/10')
}

test('signed out, Mastodon defaults fold warnings, hide sensitive media and hold GIFs still', async ({ page }) => {
  await setup(page, null)
  await expect(page.getByText('Film spoilers')).toBeVisible()
  await expect(page.getByText('What happens next')).toHaveCount(0)
  await page.getByRole('button', { name: 'Show more' }).click()
  await expect(page.getByText('What happens next')).toBeVisible()

  await expect(page.getByRole('button', { name: /Sensitive content/ })).toBeVisible()
  // Hidden media is not loaded at all, as Mastodon draws only its blurhash.
  await expect(page.getByRole('button', { name: 'View image: A blue square' })).toHaveCount(0)
  await expect(page.locator('img[src="/media/1.svg"], video[src="/media/2.mp4"]')).toHaveCount(0)
  await page.getByRole('button', { name: /Sensitive content/ }).click()
  await expect(page.getByRole('button', { name: 'View image: A blue square' })).toBeEnabled()
  const video = page.locator('video[aria-label="A spinning square"]')
  await expect(video).not.toHaveAttribute('autoplay')
  await expect(page.locator('img[src="/media/avatar.png"]').first()).toBeAttached()

  // Hide puts the cover back.
  await page.getByRole('button', { name: 'Hide' }).click()
  await expect(page.getByRole('button', { name: /Sensitive content/ })).toBeVisible()
})

test('expanding warnings, showing all media and auto-playing GIFs', async ({ page }) => {
  await setup(page, { display_media: 'show_all', expand_content_warnings: true, auto_play: true })
  await expect(page.getByText('What happens next')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Show less' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'View image: A blue square' })).toBeEnabled()
  await expect(page.getByRole('button', { name: /Sensitive content/ })).toHaveCount(0)
  await expect(page.locator('video[aria-label="A spinning square"]')).toHaveAttribute('autoplay')
  await expect(page.locator('img[src="/media/avatar.gif"]').first()).toBeAttached()
})

test('hiding all media covers media not marked sensitive', async ({ page }) => {
  await setup(
    page,
    { display_media: 'hide_all', expand_content_warnings: false, auto_play: false },
    { sensitive: false, spoiler_text: '' },
  )
  await expect(page.getByText('What happens next')).toBeVisible()
  await expect(page.getByRole('button', { name: /Media hidden/ })).toBeVisible()
  await expect(page.getByRole('button', { name: 'View image: A blue square' })).toHaveCount(0)
})

test('a saved preference is kept for the account and used on the next page', async ({ page }) => {
  await setup(page, { display_media: 'default', expand_content_warnings: false, auto_play: false })
  await expect(page.getByText('What happens next')).toHaveCount(0)
  let saved: Reading = { display_media: 'default', expand_content_warnings: false, auto_play: false }
  await page.route('**/api/eunha/v1/preferences', async (r) => {
    if (r.request().method() === 'PATCH') saved = { ...saved, ...r.request().postDataJSON() }
    await r.fulfill({ json: {
      noindex: false, show_application: true, chosen_languages: null, locale: null,
      time_zone: null, always_send_emails: false, aggregate_reblogs: true,
      notification_emails: {}, ...saved,
    } })
  })
  await page.route('**/api/eunha/v1/sessions', r => r.fulfill({ json: [] }))
  await page.goto('/settings')
  const response = page.waitForResponse(
    r => r.url().endsWith('/api/eunha/v1/preferences') && r.request().method() === 'PATCH',
  )
  await page.getByRole('switch', { name: 'Always expand posts marked with content warnings' }).click()
  await response
  await expect.poll(() => saved.expand_content_warnings).toBe(true)
  // The page has taken the answer, and cached it, once the switch shows it.
  await expect(
    page.getByRole('switch', { name: 'Always expand posts marked with content warnings' }),
  ).toBeChecked()

  // Paints from what the save cached, before (here: without) a fresh load.
  await page.route('**/api/eunha/v1/preferences', r => r.abort())
  await page.goBack()
  await expect(page.getByText('What happens next')).toBeVisible()
})
