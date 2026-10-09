import { expect, test, type Page } from '@playwright/test'

// Avatars hold still unless GIFs auto-play or the pointer is over them, as
// Mastodon's `Avatar` does: your own in the rail, the composer's suggestions,
// the invite tree and the moderation pages too. Every request is stubbed.
const account = (id: string, acct: string, name: string) => ({
  id, username: acct, acct, display_name: name, note: '',
  url: `https://example.invalid/@${acct}`, uri: `https://example.invalid/users/${acct}`,
  avatar: `/media/${acct}.gif`, avatar_static: `/media/${acct}.png`,
  header: '', header_static: '', followers_count: 0, following_count: 0, statuses_count: 0,
  created_at: '2026-01-01T00:00:00.000Z', last_status_at: null, emojis: [], fields: [],
  locked: false, bot: false, group: false,
})
const me = account('1', 'alice', 'Alice')

async function signIn(page: Page, permissions = 0, cached: Record<string, unknown> = { id: '1', acct: 'alice' }) {
  const requested: string[] = []
  await page.addInitScript((saved) => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: saved }]))
    localStorage.setItem('eunha:active-account', '1')
  }, cached)
  await page.route('**/media/**', (r) => {
    requested.push(new URL(r.request().url()).pathname)
    return r.fulfill({
      contentType: 'image/svg+xml',
      body: '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#287da8" /></svg>',
    })
  })
  await page.route('**/api/v1/accounts/verify_credentials**', (r) => r.fulfill({ json: {
    ...me, source: { privacy: 'public' }, role: { id: '3', name: 'Admin', permissions: String(permissions) },
  } }))
  await page.route('**/api/v1/notifications/unread_count**', (r) => r.fulfill({ json: { count: 0 } }))
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/announcements**', (r) => r.fulfill({ json: [] }))
  return requested
}

test('your own avatar in the rail holds still until hovered', async ({ page }) => {
  // Cached by an earlier version, before the still avatar was stored.
  const requested = await signIn(page, 0, { id: '1', acct: 'alice', displayName: 'Alice', avatar: '/media/alice.gif' })
  await page.goto('/')
  const card = page.locator('aside').getByRole('link', { name: /Alice/ })
  const image = card.locator('img')
  await expect(image).toHaveAttribute('src', '/media/alice.png')
  expect(requested).not.toContain('/media/alice.gif')
  await image.hover()
  await expect(image).toHaveAttribute('src', '/media/alice.gif')
})

test('the composer’s @mention suggestions show still avatars', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/accounts/search**', (r) => r.fulfill({ json: [account('2', 'bob', 'Bob')] }))
  await page.goto('/')
  await page.getByRole('button', { name: 'New post', exact: true }).click()
  await page.getByRole('textbox').pressSequentially('hi @bo')
  const option = page.getByRole('option', { name: /Bob/ })
  await expect(option.locator('img')).toHaveAttribute('src', '/media/bob.png')
  await option.locator('img').hover()
  await expect(option.locator('img')).toHaveAttribute('src', '/media/bob.gif')
})

test('the invite tree shows still avatars', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/eunha/v1/invite_tree', (r) => r.fulfill({ json: { total: 1, roots: [{
    id: '1', username: 'alice', acct: 'alice', display_name: 'Alice',
    avatar: '/media/alice.gif', avatar_static: '/media/alice.png',
    invited_at: '2026-01-01T00:00:00.000Z', root_reason: 'no_recorded_inviter', children: [],
  }] } }))
  await page.goto('/invite-tree')
  const image = page.getByRole('list', { name: 'Invite lineage' }).locator('img').first()
  await expect(image).toHaveAttribute('src', '/media/alice.png')
  await image.hover()
  await expect(image).toHaveAttribute('src', '/media/alice.gif')
})

test('the moderation pages show still avatars', async ({ page }) => {
  await signIn(page, 1)
  await page.route('**/api/v1/admin/accounts/9', (r) => r.fulfill({ json: {
    id: '9', username: 'far', domain: 'remote.example', created_at: '2026-01-01T00:00:00.000Z',
    email: null, ip: null, ips: [], role: { id: '-99', name: '' }, confirmed: true, suspended: false,
    silenced: false, sensitized: false, disabled: false, approved: true, locale: 'en', invite_request: null,
    account: account('9', 'far', 'Far Away'),
  } }))
  await page.route('**/api/v1/admin/account_moderation_notes?target_account_id=9', (r) => r.fulfill({ json: [] }))
  await page.goto('/admin/accounts/9')
  const image = page.locator('img[src^="/media/far"]').first()
  await expect(image).toHaveAttribute('src', '/media/far.png')
  await page.locator('[data-slot=avatar]', { has: image }).hover()
  await expect(image).toHaveAttribute('src', '/media/far.gif')
})
