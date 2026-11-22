import { expect, test, type Page } from '@playwright/test'

// How much of a post's text is drawn before "Read more" (post-content.tsx):
// Mastodon's 35-line height cut, and a length cut past which the rest is not
// drawn at all, so a post of nearly the megabyte an inbox takes cannot hang a
// timeline. Every request is stubbed.
const account = {
  id: '2', username: 'bob', acct: 'bob@troll.example', display_name: 'Bob', note: '',
  url: 'https://troll.example/@bob', uri: 'https://troll.example/users/bob',
  avatar: '', avatar_static: '', header: '', header_static: '', emojis: [], fields: [],
  followers_count: 0, following_count: 0, statuses_count: 1,
  created_at: '2026-01-01T00:00:00Z', last_status_at: null, locked: false, bot: false, group: false,
}
const status = (id: string, content: string, changes: Record<string, unknown> = {}) => ({
  id, account, content, created_at: '2026-01-01T00:00:00Z', sensitive: false, spoiler_text: '',
  visibility: 'public', language: 'en', uri: `https://troll.example/users/bob/statuses/${id}`,
  url: `https://troll.example/@bob/${id}`, replies_count: 0, reblogs_count: 0, favourites_count: 0,
  edited_at: null, media_attachments: [], mentions: [], tags: [], emojis: [], reblog: null,
  card: null, poll: null, ...changes,
})

// A hundred short lines: tall, but nowhere near the length cut.
const TALL = `<p>${Array.from({ length: 100 }, (_, i) => `line ${i + 1}`).join('<br>')}</p>`
// About 900 kB of verses, under the inbox's megabyte, ending in a marker.
const HUGE = `<p>${'In the beginning was the Word. '.repeat(29_000)}THE END</p>`

async function signIn(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/markers', (r) => r.fulfill({ json: {} }))
  await page.route('**/api/v1/invites', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/notifications/unread_count**', (r) => r.fulfill({ json: { count: 0 } }))
}

const textLength = (page: Page) =>
  page.locator('.status-content').evaluate((el) => el.textContent?.length ?? 0)

test('a tall post in the home timeline is cut to 35 lines until it is read', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [status('10', TALL)] }))
  await page.goto('/')

  const readMore = page.getByRole('button', { name: 'Read more' })
  await expect(readMore).toBeVisible()
  await expect(readMore).toHaveAttribute('aria-expanded', 'false')
  // The lines all sit in one paragraph: the box shows 35 of its 20px lines,
  // and the text under it runs on far past that.
  const box = page.locator('.post-content-cut')
  const cut = await box.evaluate((el) => ({ shown: el.getBoundingClientRect().height, whole: el.scrollHeight }))
  expect(cut.shown).toBeLessThanOrEqual(35 * 20 + 1)
  expect(cut.whole).toBeGreaterThan(90 * 20)

  await readMore.click()
  await expect(box).toHaveCount(0)
  const opened = await page.locator('.status-content').evaluate((el) => el.getBoundingClientRect().height)
  expect(opened).toBeGreaterThan(90 * 20)
  await page.getByRole('button', { name: 'Show less' }).click()
  await expect(page.getByRole('button', { name: 'Read more' })).toBeVisible()
})

test('a short post is not cut', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [status('10', '<p>Hello</p>')] }))
  await page.goto('/')
  await expect(page.getByText('Hello')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Read more' })).toHaveCount(0)
})

test('a huge post in notifications draws only its start, behind a content warning too', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/notifications?**', (r) => r.fulfill({ json: [{
    id: '42', type: 'mention', created_at: '2026-01-01T00:00:00Z', account,
    status: status('11', HUGE, { spoiler_text: 'scripture' }),
  }] }))
  await page.goto('/notifications')

  await page.getByRole('button', { name: 'Show more' }).click()
  await expect(page.getByRole('button', { name: 'Read more' })).toBeVisible()
  expect(await textLength(page)).toBeLessThanOrEqual(5000)
  await expect(page.getByText('THE END')).toHaveCount(0)

  await page.getByRole('button', { name: 'Read more' }).click()
  await expect(page.getByText('THE END')).toHaveCount(1)
  expect(await textLength(page)).toBeGreaterThan(800_000)
})

test('the focused post of a thread is drawn whole', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/statuses/12', (r) => r.fulfill({ json: status('12', TALL) }))
  await page.route('**/api/v1/statuses/12/context', (r) => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.goto('/@bob@troll.example/12')
  await expect(page.getByText('line 100')).toBeAttached()
  await expect(page.getByRole('button', { name: 'Read more' })).toHaveCount(0)
})
