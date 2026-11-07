import { expect, test, type Page } from '@playwright/test'

// The profile's tabs, as Mastodon's `AccountTabs`: Activity, with switches for
// boosts and replies and the featured hashtags to narrow by; Media; and
// Featured. Stubbed, seen by a visitor.

const account = (prefs: Record<string, boolean> = {}) => ({
  id: '7',
  username: 'carol',
  acct: 'carol',
  display_name: 'Carol',
  note: '',
  url: 'https://example.invalid/@carol',
  uri: 'https://example.invalid/users/carol',
  avatar: '',
  avatar_static: '',
  header: '',
  header_static: '',
  followers_count: 0,
  following_count: 0,
  statuses_count: 3,
  created_at: '2026-01-01T00:00:00.000Z',
  last_status_at: null,
  emojis: [],
  fields: [],
  locked: false,
  bot: false,
  group: false,
  discoverable: true,
  indexable: true,
  show_media: true,
  show_media_replies: true,
  show_featured: true,
  ...prefs,
})

const status = (id: string, content: string, extra: Record<string, unknown> = {}) => ({
  id,
  created_at: '2026-01-01T00:00:00.000Z',
  in_reply_to_id: null,
  in_reply_to_account_id: null,
  sensitive: false,
  spoiler_text: '',
  visibility: 'public',
  language: 'en',
  uri: `https://example.invalid/users/carol/statuses/${id}`,
  url: `https://example.invalid/@carol/${id}`,
  replies_count: 0,
  reblogs_count: 0,
  favourites_count: 0,
  edited_at: null,
  content: `<p>${content}</p>`,
  reblog: null,
  account: account(),
  media_attachments: [],
  mentions: [],
  tags: [],
  emojis: [],
  card: null,
  poll: null,
  ...extra,
})

const photo = (id: string, description: string) => ({
  id,
  type: 'image',
  url: '/missing.png',
  preview_url: '/missing.png',
  remote_url: null,
  description,
  blurhash: null,
  meta: null,
})

// Every timeline request the page makes, so a test can say what it asked for.
async function stub(page: Page, prefs: Record<string, boolean> = {}) {
  const asked: URLSearchParams[] = []
  await page.route('**/api/v1/accounts/lookup**', (route) =>
    route.fulfill({ json: account(prefs) }),
  )
  await page.route('**/api/v1/accounts/7/statuses**', (route) => {
    const params = new URL(route.request().url()).searchParams
    if (params.get('pinned') === 'true') return route.fulfill({ json: [] })
    asked.push(params)
    if (params.get('only_media') === 'true') {
      return route.fulfill({
        json: [
          status('11', 'a photo', { media_attachments: [photo('m1', 'A lighthouse')] }),
          status('12', 'a sensitive photo', {
            sensitive: true,
            media_attachments: [photo('m2', 'Hidden one')],
          }),
        ],
      })
    }
    if (params.get('tagged') === 'cats') {
      return route.fulfill({ json: [status('21', 'only about cats')] })
    }
    return route.fulfill({ json: [status('1', 'an ordinary post')] })
  })
  await page.route('**/api/v1/accounts/7/featured_tags**', (route) =>
    route.fulfill({
      json: [
        {
          id: '1',
          name: 'cats',
          url: 'https://example.invalid/@carol/tagged/cats',
          statuses_count: 4,
          last_status_at: '2026-01-01',
        },
      ],
    }),
  )
  await page.route('**/api/v1/accounts/7/endorsements**', (route) =>
    route.fulfill({
      json: [{ ...account(), id: '8', username: 'dave', acct: 'dave', display_name: 'Dave' }],
    }),
  )
  await page.route('**/api/v1/accounts/7/collections**', (route) =>
    route.fulfill({
      json: {
        collections: [
          {
            id: '5',
            name: 'Good follows',
            description: 'People worth reading',
            discoverable: true,
            item_count: 2,
          },
        ],
      },
    }),
  )
  return asked
}

test('the activity tab hides replies and shows boosts until switched', async ({ page }) => {
  // Cleared on the first load only, so that the reload below reads the choice.
  await page.addInitScript(() => {
    if (sessionStorage.getItem('cleared')) return
    sessionStorage.setItem('cleared', '1')
    localStorage.removeItem('eunha:account-filters')
  })
  const asked = await stub(page)

  await page.goto('/@carol')
  await expect(page.getByText('an ordinary post')).toBeVisible()
  const nav = page.getByRole('navigation', { name: 'Profile' })
  await expect(nav.getByRole('link', { name: 'Activity' })).toHaveAttribute('aria-current', 'page')
  expect(asked.at(-1)?.get('exclude_replies')).toBe('true')
  expect(asked.at(-1)?.get('exclude_reblogs')).toBe('false')

  await page.getByRole('switch', { name: 'Replies' }).click()
  await page.getByRole('switch', { name: 'Boosts' }).click()
  await expect.poll(() => asked.at(-1)?.get('exclude_replies')).toBe('false')
  expect(asked.at(-1)?.get('exclude_reblogs')).toBe('true')

  // The choice is kept for the next profile, as Mastodon keeps it.
  await page.reload()
  await expect(page.getByRole('switch', { name: 'Replies' })).toBeChecked()
  await expect(page.getByRole('switch', { name: 'Boosts' })).not.toBeChecked()
})

test('with_replies shows replies whatever the switch says', async ({ page }) => {
  await page.addInitScript(() => localStorage.removeItem('eunha:account-filters'))
  const asked = await stub(page)

  await page.goto('/@carol/with_replies')
  await expect(page.getByText('an ordinary post')).toBeVisible()
  expect(asked.at(-1)?.get('exclude_replies')).toBe('false')
  await expect(page.getByRole('switch', { name: 'Replies' })).toBeChecked()
  await expect(page.getByRole('switch', { name: 'Replies' })).toBeDisabled()
})

test('a featured hashtag narrows the activity tab, and again widens it', async ({ page }) => {
  await stub(page)

  await page.goto('/@carol')
  await page.getByRole('link', { name: /cats/ }).click()
  await expect(page).toHaveURL(/\/@carol\/tagged\/cats$/)
  await expect(page.getByText('only about cats')).toBeVisible()
  await expect(page.getByText('an ordinary post')).toBeHidden()

  await page.getByRole('link', { name: /cats/ }).click()
  await expect(page).toHaveURL(/\/@carol$/)
  await expect(page.getByText('an ordinary post')).toBeVisible()
})

test('the media tab is a grid of an account’s media, sensitive ones hidden', async ({
  page,
}) => {
  const asked = await stub(page, { show_media_replies: false })

  await page.goto('/@carol')
  await page.getByRole('navigation', { name: 'Profile' }).getByRole('link', { name: 'Media' }).click()
  await expect(page).toHaveURL(/\/@carol\/media$/)

  const grid = page.getByRole('list', { name: 'Media' })
  await expect(grid.getByRole('img', { name: 'A lighthouse' })).toBeVisible()
  // The sensitive one is drawn without its picture, and still opens its post.
  await expect(grid.getByRole('img', { name: 'Hidden one' })).toHaveCount(0)
  await expect(grid.getByRole('link', { name: 'Hidden one' })).toHaveAttribute(
    'href',
    '/@carol/12',
  )
  // Replies stay out of the grid for an account that does not show them.
  const media = asked.find((p) => p.get('only_media') === 'true')
  expect(media?.get('exclude_replies')).toBe('true')
})

test('the featured tab lists endorsed profiles and collections', async ({ page }) => {
  await stub(page)

  await page.goto('/@carol/featured')
  await expect(page.getByRole('heading', { name: 'Profiles' })).toBeVisible()
  await expect(page.getByRole('link', { name: /Dave/ })).toHaveAttribute('href', '/@dave')
  await expect(page.getByRole('heading', { name: 'Collections' })).toBeVisible()
  await expect(page.getByText('Good follows')).toBeVisible()
  await expect(page.getByText('2 accounts')).toBeVisible()
})

test('an account hiding its media and featured tabs has no tab bar', async ({ page }) => {
  await stub(page, { show_media: false, show_featured: false })

  await page.goto('/@carol/featured')
  // A hidden tab sends its visitor back to the profile.
  await expect(page).toHaveURL(/\/@carol$/)
  await expect(page.getByText('an ordinary post')).toBeVisible()
  await expect(page.getByRole('navigation', { name: 'Profile' })).toHaveCount(0)
})
