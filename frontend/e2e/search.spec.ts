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
  await expect(page.getByRole('menuitem', { name: 'Unboost' })).toBeVisible()
  // Quoting is no longer in the post's "More" menu.
  await page.keyboard.press('Escape')
  await expect(page.getByRole('menu')).toHaveCount(0)
  await page.getByRole('button', { name: 'More' }).click()
  await expect(page.getByRole('menuitem', { name: 'Quote' })).toHaveCount(0)
})

test('people found can be followed and unfollowed from the results', async ({ page }) => {
  await signedIn(page)
  const carol = account('3', 'carol')
  await page.route('**/api/v2/search**', (r) => {
    const type = new URL(r.request().url()).searchParams.get('type')
    const empty = { accounts: [], hashtags: [], statuses: [] }
    return r.fulfill({ json: type === 'accounts' ? { ...empty, accounts: [me, carol] } : empty })
  })
  const relationship = (id: string, following: boolean) => ({
    id, following, requested: false, followed_by: false, blocking: false, blocked_by: false,
    muting: false, muting_notifications: false, domain_blocking: false, endorsed: false,
    showing_reblogs: true, notifying: false, languages: null, note: '',
  })
  let asked: string[] = []
  await page.route('**/api/v1/accounts/relationships**', (r) => {
    asked = new URL(r.request().url()).searchParams.getAll('id[]')
    return r.fulfill({ json: asked.map((id) => relationship(id, false)) })
  })
  let releaseFollow = () => {}
  const followHeld = new Promise<void>((resolve) => (releaseFollow = resolve))
  await page.route('**/api/v1/accounts/3/follow', async (r) => {
    await followHeld
    return r.fulfill({ json: relationship('3', true) })
  })
  await page.route('**/api/v1/accounts/3/unfollow', (r) => r.fulfill({ json: relationship('3', false) }))

  await page.goto('/search?q=a')
  const people = page.locator('section', { has: page.getByRole('heading', { name: 'People' }) })
  // One button, for carol: the signed-in account is not offered to itself.
  await expect(people.getByRole('button', { name: 'Follow' })).toHaveCount(1)
  expect(asked).toEqual(['3'])
  await people.getByRole('button', { name: 'Follow' }).click()
  // A spinner while the follow is on its way.
  const sending = people.locator('button[aria-busy="true"]')
  await expect(sending.locator('[data-slot="spinner"]')).toBeVisible()
  await expect(sending).toBeDisabled()
  releaseFollow()
  await expect(people.getByRole('button', { name: 'Following' })).toBeVisible()
  await expect(people.locator('[data-slot="spinner"]')).toHaveCount(0)
  await people.getByRole('button', { name: 'Following' }).click()
  await expect(people.getByRole('button', { name: 'Follow' })).toBeVisible()
  await expect(page).toHaveURL(/\/search\?q=a$/)
})

test('a follow request found in the results turns into Following once accepted', async ({ page }) => {
  await signedIn(page)
  await page.clock.install()
  const dave = { ...account('4', 'dave@remote.example'), locked: true }
  await page.route('**/api/v2/search**', (r) => {
    const type = new URL(r.request().url()).searchParams.get('type')
    const empty = { accounts: [], hashtags: [], statuses: [] }
    return r.fulfill({ json: type === 'accounts' ? { ...empty, accounts: [dave] } : empty })
  })
  const relationship = (state: 'none' | 'requested' | 'following') => ({
    id: '4', following: state === 'following', requested: state === 'requested',
    followed_by: false, blocking: false, blocked_by: false, muting: false,
    muting_notifications: false, domain_blocking: false, endorsed: false,
    showing_reblogs: true, notifying: false, languages: null, note: '',
  })
  let state: 'none' | 'requested' | 'following' = 'none'
  await page.route('**/api/v1/accounts/relationships**', (r) => r.fulfill({ json: [relationship(state)] }))
  await page.route('**/api/v1/accounts/4/follow', (r) => {
    state = 'requested'
    return r.fulfill({ json: relationship(state) })
  })

  await page.goto('/search?q=dave')
  // Gone if the page reloads.
  await page.evaluate(() => ((window as unknown as { sameDocument: boolean }).sameDocument = true))
  const people = page.locator('section', { has: page.getByRole('heading', { name: 'People' }) })
  await people.getByRole('button', { name: 'Follow' }).click()
  await expect(people.getByRole('button', { name: 'Requested' })).toBeVisible()

  // Still pending at the first check.
  await page.clock.runFor(2_000)
  await expect(people.getByRole('button', { name: 'Requested' })).toBeVisible()

  state = 'following'
  await page.clock.runFor(4_000)
  await expect(people.getByRole('button', { name: 'Following' })).toBeVisible()
  expect(await page.evaluate(() => (window as unknown as { sameDocument?: boolean }).sameDocument)).toBe(true)
})
