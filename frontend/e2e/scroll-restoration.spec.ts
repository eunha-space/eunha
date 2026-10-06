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

for (const mobile of [false, true]) {
  for (const profile of [false, true]) {
    test(`back restores paginated ${profile ? 'profile posts' : 'timeline'} and scroll on ${mobile ? 'mobile' : 'desktop'}`, async ({ page }) => {
      if (mobile) await page.setViewportSize({ width: 390, height: 844 })
      await page.route('**/api/v2/instance', r => r.fulfill({ json: { title: 'Example', domain: 'example.invalid', registrations: { enabled: false } } }))
      await page.route('**/api/v1/accounts/lookup**', r => r.fulfill({ json: account }))
      let feedRequests = 0
      const feedRoute = profile ? '**/api/v1/accounts/1/statuses**' : '**/api/v1/timelines/public**'
      await page.route(feedRoute, r => {
        const url = new URL(r.request().url())
        if (url.searchParams.has('pinned')) return r.fulfill({ json: [] })
        feedRequests++
        const start = url.searchParams.has('max_id') ? 20 : 40
        return r.fulfill({ json: Array.from({ length: 20 }, (_, i) => status(String(start - i), `Post ${start - i}`, false)) })
      })
      await page.route('**/api/v1/statuses/*/context', r => r.fulfill({ json: { ancestors: [], descendants: [] } }))
      await page.route('**/api/v1/statuses/10', r => r.fulfill({ json: status('10', 'Post 10', false) }))
      const path = profile ? '/@alice' : '/local'
      await page.goto(path)
      const post = page.locator('a[href="/@alice/10"]').first()
      await expect(page.getByText('Post 40', { exact: true })).toBeVisible()
      await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight))
      await expect(page.getByText('Post 10', { exact: true })).toBeAttached()
      await post.scrollIntoViewIfNeeded()
      const offset = await page.evaluate(() => window.scrollY)
      expect(offset).toBeGreaterThan(1000)
      await post.click()
      await expect(page).toHaveURL(/\/@alice\/10$/)
      await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(0)
      const requests = feedRequests
      await page.goBack()
      await expect(page).toHaveURL(new RegExp(`${path}$`))
      await expect(page.getByText('Post 10', { exact: true })).toBeAttached()
      await expect.poll(async () => Math.abs(await page.evaluate(() => window.scrollY) - offset)).toBeLessThan(3)
      expect(feedRequests).toBe(requests)
      await page.goForward()
      await expect(page).toHaveURL(/\/@alice\/10$/)
      await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(0)
      await page.goBack()
      await expect(page).toHaveURL(new RegExp(`${path}$`))
      await expect.poll(async () => Math.abs(await page.evaluate(() => window.scrollY) - offset)).toBeLessThan(3)
    })
  }
}
