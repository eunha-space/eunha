import { expect, test } from '@playwright/test'

test('search submits the query into the URL', async ({ page }) => {
  await page.goto('/search')
  await page.getByPlaceholder('Search posts, people, and hashtags').fill('hello')
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/search\?q=hello$/)
})

const account = (id: string, acct: string) => ({
  id, username: acct.split('@')[0], acct, display_name: acct, note: '',
  url: `https://remote.example/@${acct}`, uri: `https://remote.example/users/${acct}`,
  avatar: '', avatar_static: '', header: '', header_static: '', emojis: [], fields: [],
  followers_count: 0, following_count: 0, statuses_count: 1,
  created_at: '2026-01-01T00:00:00Z', locked: false, bot: false, group: false,
})
const me = account('1', 'alice')
const bob = account('2', 'bob@remote.example')
const remotePost = {
  id: '20', account: bob, content: '<p>A remote post</p>', created_at: '2026-01-01T00:00:00Z',
  sensitive: false, spoiler_text: '', visibility: 'public', language: 'en',
  uri: 'https://remote.example/users/bob/statuses/20', url: 'https://remote.example/@bob/20',
  replies_count: 0, reblogs_count: 0, favourites_count: 0, edited_at: null,
  media_attachments: [], mentions: [], tags: [], emojis: [], reblog: null, card: null,
  poll: null, quote: null, reblogged: false, quote_approval: { automatic: ['public'], manual: [], current_user: 'automatic' },
}

async function signedIn(page: import('@playwright/test').Page) {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/verify_credentials**', (r) =>
    r.fulfill({ json: { ...me, source: { privacy: 'public' } } }),
  )
  await page.route('**/api/v1/notifications/unread_count**', (r) => r.fulfill({ json: { count: 0 } }))
}

test('search shows each part as it finishes, the remote lookup included', async ({ page }) => {
  await signedIn(page)
  let releasePosts = () => {}
  const postsHeld = new Promise<void>((resolve) => (releasePosts = resolve))
  await page.route('**/api/v2/search**', async (r) => {
    const url = new URL(r.request().url())
    const type = url.searchParams.get('type')
    const resolve = url.searchParams.get('resolve') === 'true'
    const empty = { accounts: [], hashtags: [], statuses: [] }
    if (type === 'statuses') {
      await postsHeld
      return r.fulfill({ json: { ...empty, statuses: [remotePost] } })
    }
    if (type === 'accounts' && resolve) return r.fulfill({ json: { ...empty, accounts: [bob] } })
    return r.fulfill({ json: empty })
  })

  await page.goto('/search?q=bob@remote.example')
  const progress = page.getByRole('list', { name: 'Search progress' })
  await expect(progress.getByText('Looking up @bob@remote.example on remote.example · 1 found')).toBeVisible()
  await expect(progress.getByText('People on this server · nothing found')).toBeVisible()
  await expect(progress.getByText('Posts…')).toBeVisible()
  // What came back already shows while the post search runs.
  await expect(page.getByRole('heading', { name: 'People' })).toBeVisible()

  releasePosts()
  await expect(progress.getByText('Posts · 1 found')).toBeVisible()
  await expect(page.getByText('A remote post')).toBeVisible()
})

test('the boost button offers boosting and quoting, and quoting a found post opens the composer', async ({ page }) => {
  await signedIn(page)
  await page.route('**/api/v2/search**', (r) =>
    r.fulfill({ json: { accounts: [], hashtags: [], statuses: [remotePost] } }),
  )
  let boosted = false
  await page.route('**/api/v1/statuses/20/reblog', (r) => {
    boosted = true
    return r.fulfill({ json: { ...remotePost, id: '21', reblog: { ...remotePost, reblogged: true } } })
  })

  await page.goto('/search?q=remote')
  await page.getByRole('button', { name: 'Boost or quote' }).click()
  await expect(page.getByRole('menuitem', { name: 'Boost' })).toBeVisible()
  await page.getByRole('menuitem', { name: 'Quote' }).click()
  await expect(page.getByRole('heading', { name: 'Quote post' })).toBeVisible()
  expect(boosted).toBe(false)
  await page.getByRole('button', { name: 'Close' }).click()

  await page.getByRole('button', { name: 'Boost or quote' }).click()
  await page.getByRole('menuitem', { name: 'Boost' }).click()
  await expect.poll(() => boosted).toBe(true)
  await expect(page.getByRole('menu')).toHaveCount(0)
  await page.getByRole('button', { name: 'Boost or quote' }).click()
  await expect(page.getByRole('menuitem', { name: 'Undo boost' })).toBeVisible()
  // Quoting is no longer in the post's "More" menu.
  await page.keyboard.press('Escape')
  await expect(page.getByRole('menu')).toHaveCount(0)
  await page.getByRole('button', { name: 'More' }).click()
  await expect(page.getByRole('menuitem', { name: 'Quote' })).toHaveCount(0)
})
