import { expect, test, type Page } from '@playwright/test'

const instance = {
  domain: 'example.invalid', title: 'Example', version: '4.7.3',
  description: '', configuration: {}, languages: ['en'], rules: [],
  usage: { users: { active_month: 2 } }, contact: { email: '', account: null },
  registrations: { enabled: true, approval_required: true, reason_required: true },
}

async function signIn(page: Page, permissions = (1 << 11) | (1 << 16)) {
  await page.addInitScript(() => {
    const account = { id: '1', acct: 'alice' }
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account }]))
    localStorage.setItem('eunha:active-account', '1')
    localStorage.setItem('eunha:me-id', '1')
    localStorage.setItem('eunha:me-account', JSON.stringify(account))
  })
  await page.route('**/api/v1/accounts/verify_credentials**', r => r.fulfill({ json: {
    id: '1', username: 'alice', acct: 'alice', display_name: 'Alice', note: '',
    avatar: '', avatar_static: '', header: '', header_static: '', emojis: [], fields: [],
    url: 'https://example.invalid/@alice', created_at: '2026-01-01T00:00:00Z',
    followers_count: 0, following_count: 0, statuses_count: 0, source: { privacy: 'public' },
    role: { id: '3', name: 'Admin', permissions: String(permissions) },
  } }))
  await page.route('**/api/v1/notifications/unread_count**', r => r.fulfill({ json: { count: 0 } }))
  await page.route('**/api/v2/instance', r => r.fulfill({ json: instance }))
  await page.route('**/api/eunha/v1/terms_of_service/interstitial', r => r.fulfill({ status: 204 }))
}

const invite = { id: '10', code: 'abc', url: 'https://example.invalid/invite/abc',
  uses: 0, max_uses: 1, expires_at: null, expired: false, valid_for_use: true,
  bypass_approval: true, grant: null, autofollow: false, comment: null, created_at: '2026-01-01T00:00:00Z',
  joined: { accounts: [], hidden: 0, pending: 0 } }
type Nested = { id: string; kind?: string; children?: Nested[]; [key: string]: unknown }
const member: Nested = { id: '1', kind: 'member', username: 'alice', acct: 'alice', display_name: 'Alice', avatar: '',
  joined_at: '2026-01-01T00:00:00Z', children: [] }
/** The server's flat shape: parents first, each naming its parent. */
function flat(roots: Nested[], parent: string | null = null): object[] {
  return roots.flatMap(({ children = [], ...node }) => [
    { kind: 'member', ...node, parent_id: parent },
    ...flat(children, node.id),
  ])
}
/** What `?scope=branch` serves: the viewer's line and everyone below. */
function branchOf(members: { id: string; parent_id: string | null }[], me: string) {
  const parents = new Map(members.map(m => [m.id, m.parent_id]))
  const keep = new Set<string>()
  for (let at: string | null | undefined = me; at; at = parents.get(at)) keep.add(at)
  const below = new Set([me])
  for (const m of members) if (m.parent_id && below.has(m.parent_id)) { below.add(m.id); keep.add(m.id) }
  return members.filter(m => keep.has(m.id))
}
async function serveTree(page: Page, total: number, roots: Nested[]) {
  const members = flat(roots) as { id: string; parent_id: string | null }[]
  await page.route('**/api/eunha/v1/invite_tree*', r => r.fulfill({ json: {
    total, members: new URL(r.request().url()).searchParams.get('scope') === 'branch' ? branchOf(members, '1') : members,
  } }))
}

test('personal invites retain revoked history and distinguish fully used links', async ({ page }) => {
  await signIn(page, 1 << 16)
  let revoked = false
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [{ ...invite, expired: revoked, valid_for_use: !revoked }, { ...invite, id: '11', uses: 1, valid_for_use: false }] }))
  await page.route('**/api/v1/invites/10', r => { revoked = true; return r.fulfill({ status: 204 }) })
  await page.goto('/invites')
  await page.getByText('Manage links', { exact: false }).click()
  await expect(page.getByRole('button', { name: 'Revoke invite' })).toHaveCount(1)
  await page.getByRole('button', { name: 'Revoke invite' }).click()
  await page.getByRole('button', { name: 'Expired', exact: true }).click()
  await expect(page.getByLabel('Invite link', { exact: true })).toHaveCount(1)
  await page.getByRole('button', { name: 'Fully used', exact: true }).click()
  await expect(page.getByLabel('Invite link', { exact: true })).toHaveCount(1)
  await expect(page.getByRole('button', { name: 'Revoke invite' })).toHaveCount(0)
})

test('grant recipients are explicit and bulk grants show their reach', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
  await page.route('**/api/eunha/v1/invite_grants/recipients', r => r.fulfill({ json: [member] }))
  await page.route('**/api/v1/admin/invites**', r => r.fulfill({ json: [] }))
  let submitted: unknown
  await page.route('**/api/eunha/v1/invite_grants', r => {
    if (r.request().method() === 'GET') return r.fulfill({ json: [] })
    submitted = r.request().postDataJSON()
    return r.fulfill({ json: { granted: 1, accounts: 1 } })
  })
  await page.goto('/admin/invites')
  const review = page.getByRole('button', { name: 'Review grant', exact: true })
  await expect(review).toBeDisabled()
  await page.getByLabel('Search members').fill('alice')
  await page.getByRole('combobox', { name: 'Recipient' }).click()
  await page.getByRole('option', { name: 'All local users (1)' }).click()
  await page.getByLabel('People each member can invite').fill('2')
  await expect(page.getByText('Create 2 single-use links for 1 local user.')).toBeVisible()
  await expect(page.getByLabel(/Note/)).toHaveCount(0)
  await review.click()
  await expect.poll(() => submitted).toBeUndefined()
  await expect(page.getByText('No staff approval needed. Email confirmation required.', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Grant 2 invites', exact: true }).click()
  await expect.poll(() => submitted).toEqual({ all: true, count: 2, max_uses: 1, expires_in: 604800 })

})

test('grant member lookup failure disables grants and offers retry', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
  let failed = true
  await page.route('**/api/eunha/v1/invite_grants/recipients', r => failed ? r.fulfill({ status: 500 }) : r.fulfill({ json: [member] }))
  await page.route('**/api/v1/admin/invites**', r => r.fulfill({ json: [] }))
  await page.route('**/api/eunha/v1/invite_grants', r => r.fulfill({ json: [] }))
  await page.goto('/admin/invites?grant_to=1')
  await expect(page.getByText('Could not load members.', { exact: false })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Review grant', exact: true })).toBeDisabled()
  failed = false
  await page.getByRole('button', { name: 'Try again' }).click()
  await expect(page.getByRole('button', { name: 'Review grant', exact: true })).toBeEnabled()
})

test('admin invite list exposes URLs and respects validity', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/eunha/v1/invite_grants', r => r.fulfill({ json: [] }))
  await page.route('**/api/v1/admin/invites**', r => r.fulfill({ json: [
    { ...invite, account: null }, { ...invite, id: '11', uses: 1, valid_for_use: false, account: null },
  ] }))
  await page.goto('/admin/invites')
  await expect(page.getByText('Fully used', { exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Expire', exact: true })).toHaveCount(1)
  await expect(page.getByRole('button', { name: 'Copy link' }).first()).toBeEnabled()
  await expect(page.getByRole('button', { name: 'Copy link' }).last()).toBeDisabled()
  await expect(page.getByRole('link', { name: 'Create invite', exact: true })).toBeVisible()
})

const tree: Nested[] = [{ ...member, id: '0', acct: 'founder', username: 'founder', display_name: 'Founder',
  root_reason: 'no_recorded_inviter', children: [{ ...member, children: [
    { ...member, id: '2', acct: 'bob', username: 'bob', display_name: 'Bob', children: [
      { ...member, id: '4', acct: 'carol', username: 'carol', display_name: 'Carol' },
    ] },
  ] }, { ...member, id: '3', acct: 'sibling', username: 'sibling', display_name: 'Sibling' }] }]

test('tree starts with my ancestry and supports collapse, search and finding me', async ({ page }) => {
  await signIn(page)
  await serveTree(page, 5, tree)
  await page.goto('/invite-tree')
  const lineage = page.getByRole('tree', { name: 'Invite lineage' })
  await expect(lineage.getByRole('link', { name: 'Founder @founder' })).toBeVisible()
  await expect(lineage.getByRole('link', { name: 'Bob @bob' })).toBeVisible()
  await expect(lineage.getByRole('link', { name: 'Sibling @sibling' })).toHaveCount(0)
  await expect(lineage.getByRole('link', { name: 'Carol @carol' })).toHaveCount(0)
  await page.getByRole('button', { name: 'Expand @bob' }).click()
  await expect(lineage.getByRole('link', { name: 'Carol @carol' })).toBeVisible()
  await page.getByRole('button', { name: 'Collapse all' }).click()
  await expect(lineage.getByRole('link', { name: 'Bob @bob' })).toHaveCount(0)
  await page.getByLabel('Search members').fill('carol')
  await expect(lineage.getByRole('link', { name: 'Carol @carol' })).toBeVisible()
  await expect(lineage.getByRole('link', { name: 'Founder @founder' })).toBeVisible()
  await page.getByRole('button', { name: 'Find me' }).click()
  await expect(page.getByLabel('Search members')).toHaveValue('')
  await expect(page.locator('#invite-member-1')).toBeFocused()
  await expect(page).toHaveURL(/member=1/)
  await page.getByRole('button', { name: 'Whole instance' }).click()
  await expect(page.getByRole('button', { name: 'Whole instance' })).toHaveAttribute('aria-pressed', 'true')
  await expect(page).toHaveURL(/view=all/)
  await expect(lineage.getByRole('link', { name: 'Sibling @sibling' })).toBeVisible()
  await lineage.getByRole('link', { name: 'Hand out invites to @alice' }).click()
  await expect(page).toHaveURL(/\/admin\/invites\?grant_to=1$/)
})

test('tree keeps a former member\'s branch and a deep search usable on mobile', async ({ page }) => {
  await signIn(page, 1 << 16)
  await page.setViewportSize({ width: 390, height: 844 })
  let chain: Nested = { ...member, id: '20', acct: 'last', username: 'last', display_name: 'Last' }
  for (let i = 19; i >= 0; i--) chain = { ...member, id: String(i), acct: `member${i}`, username: `member${i}`, display_name: `Member ${i}`, children: [chain] }
  await serveTree(page, 21, [{ id: 'former-1', kind: 'former_member', children: [chain] }])
  await page.goto('/invite-tree')
  await expect(page.getByText('A former member', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Whole instance' }).click()
  await page.getByLabel('Search members').fill('last')
  await expect(page.getByRole('link', { name: 'Last @last' })).toBeVisible()
  await expect(page.getByRole('link', { name: /Hand out invites to/ })).toHaveCount(0)
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: '/tmp/eunha-invite-tree-mobile.png', fullPage: true })
})

test('tree empty search and load failure have clear recovery states', async ({ page }) => {
  await signIn(page)
  let failed = true
  const members = flat(tree) as { id: string; parent_id: string | null }[]
  await page.route('**/api/eunha/v1/invite_tree*', r => failed ? r.fulfill({ status: 500 })
    : r.fulfill({ json: { total: 5, members: branchOf(members, '1') } }))
  await page.goto('/invite-tree')
  await expect(page.getByRole('alert')).toContainText('Could not load the invite tree')
  failed = false
  await page.getByRole('button', { name: 'Try again' }).click()
  await expect(page.getByRole('button', { name: 'My branch' })).toBeEnabled()
  await page.getByLabel('Search members').fill('nobody')
  await expect(page.getByText('No matching members in this view.')).toBeVisible()
})

for (const permissions of [0, 1 << 16]) {
  for (const mobile of [false, true]) {
    test(`invite navigation is visible with permission ${permissions} on ${mobile ? 'mobile' : 'desktop'}`, async ({ page }) => {
      await signIn(page, permissions)
      if (mobile) await page.setViewportSize({ width: 390, height: 844 })
      await page.route('**/api/v1/invites', r => r.fulfill({ json: [
        invite,
        { ...invite, id: '11', valid_for_use: false, uses: 1 },
        { ...invite, id: '12', valid_for_use: false, expired: true },
        { ...invite, id: '13', valid_for_use: false },
      ] }))
      await page.goto('/about')
      if (mobile) await page.getByRole('button', { name: 'Open menu' }).click()
      const navigation = mobile ? page.getByRole('dialog') : page.locator('aside')
      const name = 'Invite people'
      const link = navigation.getByRole('link', { name: new RegExp(name) })
      await expect(link).toBeVisible()
      await expect(link.locator('span[aria-label]')).toHaveAttribute('aria-label', '1 available single-use invite links')
      await link.click()
      await expect(page).toHaveURL(/\/invites$/)
      if (mobile) await expect(page.getByRole('dialog')).toHaveCount(0)
    })
  }
}

test('visitors do not see invite navigation', async ({ page }) => {
  await page.route('**/api/v2/instance', r => r.fulfill({ json: instance }))
  await page.goto('/about')
  await expect(page.locator('aside').getByRole('link', { name: /Invite people|Your invites/ })).toHaveCount(0)
})


test('member sharing preserves grant batches and copying does not consume links', async ({ page, context }) => {
  await signIn(page, 0)
  await context.grantPermissions(['clipboard-read', 'clipboard-write'])
  const grantA = { id: '1', granted_by: 'mira', created_at: '2026-01-01T00:00:00Z' }
  const grantB = { id: '2', granted_by: 'jun', created_at: '2026-02-01T00:00:00Z' }
  const links = [
    { ...invite, grant: grantA, expires_at: '2090-01-01T00:00:00Z' },
    { ...invite, id: '11', code: 'def', url: 'https://example.invalid/signup?invite=def', grant: grantA, expires_at: '2090-01-01T00:00:00Z' },
    { ...invite, id: '12', code: 'ghi', url: 'https://example.invalid/signup?invite=ghi', grant: grantB, expires_at: '2090-02-01T00:00:00Z' },
    { ...invite, id: '13', grant: grantA, uses: 1, valid_for_use: false },
  ]
  let list = links
  await page.route('**/api/v1/invites', r => r.fulfill({ json: list }))
  await page.goto('/invites')
  await expect(page.getByRole('heading', { name: 'Invite 3 people' })).toBeVisible()
  await expect(page.getByLabel('Selected invite link')).toHaveValue(invite.url)
  await page.getByRole('button', { name: 'Copy invite link', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Copy this link again' })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Invite 3 people' })).toBeVisible()
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(invite.url)
  await page.getByRole('button', { name: 'Choose another link' }).click()
  await expect(page.getByLabel('Selected invite link')).toHaveValue(links[1].url)
  await page.getByRole('button', { name: 'Choose another link' }).click()
  await expect(page.getByLabel('Selected invite link')).toHaveValue(links[2].url)
  // A new grant must not replace the selected link or merge with a previous batch.
  list = [...links, { ...invite, id: '14', grant: { ...grantA, id: '3' } }]
  await page.evaluate(() => window.dispatchEvent(new Event('eunha:invites-changed')))
  await expect(page.getByRole('heading', { name: 'Invite 4 people' })).toBeVisible()
  await expect(page.getByLabel('Selected invite link')).toHaveValue(links[2].url)
  await expect(page.getByRole('region', { name: 'Invites from staff' })).toHaveCount(0)
  await page.getByText('Manage links', { exact: false }).click()
  await expect(page.locator('details').getByRole('heading', { level: 3 })).toHaveCount(3)
  await page.getByRole('button', { name: 'Fully used', exact: true }).click()
  await expect(page.getByLabel('Invite link', { exact: true })).toHaveCount(1)
  await page.setViewportSize({ width: 360, height: 800 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  await page.screenshot({ path: '/tmp/eunha-member-invites-mobile.png', fullPage: true })
})

test('unlimited links hide the sidebar count and do not imply a people allowance', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [{ ...invite, max_uses: null }] }))
  await page.goto('/invites')
  await expect(page.getByRole('heading', { name: '1 invite link available' })).toBeVisible()
  await expect(page.getByText('This link allows unlimited signups.', { exact: false })).toBeVisible()
  await expect(page.locator('aside span[aria-label$="available single-use invite links"]')).toHaveCount(0)
  await expect(page.locator('aside').getByRole('link', { name: 'Invite people', exact: true })).toBeVisible()
})

test('available link expires while open and removes the sidebar count', async ({ page }) => {
  await signIn(page, 0)
  await page.clock.install({ time: new Date('2026-10-05T00:00:00Z') })
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [{ ...invite, expires_at: '2026-10-05T00:00:02Z' }] }))
  await page.goto('/invites')
  await expect(page.getByRole('heading', { name: 'Invite 1 person' })).toBeVisible()
  await page.clock.fastForward(3000)
  await expect(page.getByRole('heading', { name: 'No invites available' })).toBeVisible()
  await expect(page.locator('aside').getByRole('link', { name: /Invite people/ })).toHaveCount(0)
  await expect(page.locator('aside span[aria-label$="available single-use invite links"]')).toHaveCount(0)
})


test('failed grant can be retried from review without changing the recipient', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
  await page.route('**/api/v1/admin/invites**', r => r.fulfill({ json: [] }))
  await page.route('**/api/eunha/v1/invite_grants/recipients', r => r.fulfill({ json: [member] }))
  let failed = true
  await page.route('**/api/eunha/v1/invite_grants', r => r.request().method() === 'GET' ? r.fulfill({ json: [] })
    : failed ? r.fulfill({ status: 500 }) : r.fulfill({ json: { granted: 3, accounts: 1 } }))
  await page.goto('/admin/invites?grant_to=1')
  await page.getByRole('button', { name: 'Review grant' }).click()
  const grant = page.getByRole('button', { name: 'Grant 3 invites', exact: true })
  await grant.click()
  await expect(page.getByRole('alert')).toContainText('Could not grant invites')
  await expect(grant).toBeEnabled()
  failed = false
  await grant.click()
  await expect(page.getByRole('heading', { name: '3 invites granted to 1 local user' })).toBeVisible()
})


for (const mobile of [false, true]) {
  test(`invite menu hides without usable links and appears after a grant on ${mobile ? 'mobile' : 'desktop'}`, async ({ page }) => {
    await signIn(page, 1 << 16)
    if (mobile) await page.setViewportSize({ width: 390, height: 844 })
    let granted = false
    await page.route('**/api/v1/invites', r => r.fulfill({ json: granted ? [invite] : [
      { ...invite, id: '11', uses: 1, valid_for_use: false },
      { ...invite, id: '12', expired: true, valid_for_use: false },
      { ...invite, id: '13', valid_for_use: false },
    ] }))
    await page.goto('/about')
    if (mobile) await page.getByRole('button', { name: 'Open menu' }).click()
    const navigation = mobile ? page.getByRole('dialog') : page.locator('aside')
    const link = navigation.getByRole('link', { name: /Invite people/ })
    await expect(link).toHaveCount(0)
    granted = true
    await page.evaluate(() => window.dispatchEvent(new Event('eunha:invites-changed')))
    await expect(link).toBeVisible()
    granted = false
    await page.evaluate(() => window.dispatchEvent(new Event('eunha:invites-changed')))
    await expect(link).toHaveCount(0)
  })
}


for (const mobile of [false, true]) {
  test(`moderator keeps invite menu without usable links on ${mobile ? 'mobile' : 'desktop'}`, async ({ page }) => {
    await signIn(page, 1 << 4)
    if (mobile) await page.setViewportSize({ width: 390, height: 844 })
    await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
    await page.goto('/about')
    if (mobile) await page.getByRole('button', { name: 'Open menu' }).click()
    const navigation = mobile ? page.getByRole('dialog') : page.locator('aside')
    await expect(navigation.getByRole('link', { name: 'Invite people', exact: true })).toBeVisible()
    await expect(navigation.locator('span[aria-label$="available single-use invite links"]')).toHaveCount(0)
  })
}


test('invite menu survives navigation and failed refreshes, then updates availability', async ({ page }) => {
  await signIn(page, 0)
  let mode: 'available' | 'pending' | 'failed' | 'empty' = 'available'
  let pendingRequests = 0
  let release!: () => void
  const pending = new Promise<void>(resolve => { release = resolve })
  await page.route('**/api/v1/invites', async r => {
    if (mode === 'pending') {
      pendingRequests++
      await pending
    }
    if (mode === 'failed') return r.fulfill({ status: 500 })
    return r.fulfill({ json: mode === 'empty' ? [] : [invite] })
  })
  await page.goto('/about')
  const link = page.locator('aside').getByRole('link', { name: /Invite people/ })
  await expect(link).toBeVisible()
  mode = 'pending'
  await page.locator('aside').getByRole('link', { name: 'Search', exact: true }).click()
  await expect(page).toHaveURL(/\/search$/)
  await expect.poll(() => pendingRequests).toBeGreaterThan(0)
  await expect(link).toBeVisible()
  await expect(link).toContainText('1')
  mode = 'failed'
  const failedResponse = page.waitForResponse(r => r.url().endsWith('/api/v1/invites') && r.status() === 500)
  release()
  await failedResponse
  await expect(link).toBeVisible()
  mode = 'empty'
  await page.evaluate(() => window.dispatchEvent(new Event('eunha:invites-changed')))
  await expect(link).toHaveCount(0)
})

test('hidden members keep their branch without a name, and the tree works from the keyboard', async ({ page }) => {
  await signIn(page, 0)
  await serveTree(page, 4, [{ ...member, children: [
    { id: 'hidden-1', kind: 'hidden_member', children: [
      { ...member, id: '4', acct: 'carol', username: 'carol', display_name: 'Carol' },
    ] },
    { ...member, id: '5', acct: 'dave', username: 'dave', display_name: 'Dave' },
  ] }])
  await page.goto('/invite-tree?view=all')
  const lineage = page.getByRole('tree', { name: 'Invite lineage' })
  await expect(lineage.getByText('A member who hides their network')).toBeVisible()
  await expect(lineage.getByText('2 direct invitees, 3 in all')).toBeVisible()
  await page.locator('#invite-member-1').focus()
  await page.keyboard.press('ArrowDown')
  await expect(page.locator('#invite-member-hidden-1')).toBeFocused()
  await page.keyboard.press('ArrowRight')
  await expect(lineage.getByRole('link', { name: 'Carol @carol' })).toBeVisible()
  await page.keyboard.press('ArrowRight')
  await expect(page.locator('#invite-member-4')).toBeFocused()
  await page.keyboard.press('ArrowLeft')
  await expect(page.locator('#invite-member-hidden-1')).toBeFocused()
  await page.keyboard.press('End')
  await expect(page.locator('#invite-member-5')).toBeFocused()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/@dave$/)
})

test('a link to a member in the tree opens the way to them', async ({ page }) => {
  await signIn(page, 0)
  await serveTree(page, 5, tree)
  await page.goto('/invite-tree?view=all&member=4')
  await expect(page.locator('#invite-member-4')).toBeFocused()
  await expect(page.getByRole('link', { name: 'Carol @carol' })).toBeVisible()
})

test('each link says who joined through it', async ({ page }) => {
  await signIn(page, 0)
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [{ ...invite, uses: 1, max_uses: 3,
    joined: { accounts: [{ id: '2', acct: 'bob' }], hidden: 1, pending: 1 } }] }))
  await page.goto('/invites')
  await page.getByText('Manage links').click()
  await expect(page.getByRole('link', { name: '@bob' }).first()).toBeVisible()
  await expect(page.getByText('1 member who hides their network').first()).toBeVisible()
  await expect(page.getByText(/1 not finished joining/).first()).toBeVisible()
})

test('staff review handouts and revoke one', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/admin/invites**', r => r.fulfill({ json: [] }))
  await page.route('**/api/eunha/v1/invite_grants/recipients', r => r.fulfill({ json: [member] }))
  let revoked = false
  const grant = { id: '7', created_at: '2026-01-01T00:00:00', granted_by: 'alice', revoked_by: null,
    expires_at: null, recipient: 'bob', recipients: 1, links: 2, uses: 1, available: 1 }
  await page.route('**/api/eunha/v1/invite_grants', r => r.fulfill({ json: [
    revoked ? { ...grant, revoked_at: '2026-01-02T00:00:00', revoked_by: 'alice', available: 0 } : { ...grant, revoked_at: null },
  ] }))
  await page.route('**/api/eunha/v1/invite_grants/7/revoke', r => { revoked = true; return r.fulfill({ json: { expired: 1 } }) })
  await page.goto('/admin/invites')
  const handouts = page.getByRole('region', { name: 'Handouts' })
  await expect(handouts.getByText('2 links to @bob')).toBeVisible()
  await handouts.getByRole('button', { name: 'Revoke' }).click()
  await page.getByRole('alertdialog').getByRole('button', { name: 'Revoke' }).click()
  await expect(handouts.getByText(/Revoked by @alice/)).toBeVisible()
})
