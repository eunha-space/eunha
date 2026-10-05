import { expect, test, type Page } from '@playwright/test'

const instance = {
  domain: 'example.invalid', title: 'Example', version: '4.7.1',
  description: '', configuration: {}, languages: ['en'], rules: [],
  usage: { users: { active_month: 2 } }, contact: { email: '', account: null },
  registrations: { enabled: true, approval_required: true, reason_required: true },
}

for (const bypass of [false, true]) {
  test(`invite signup respects approval bypass ${bypass}`, async ({ page }) => {
    await page.route('**/api/v2/instance', r => r.fulfill({ json: instance }))
    await page.route('**/api/eunha/v1/invite?*', r => r.fulfill({ json: {
      valid: true, bypass_approval: bypass, autofollow: true,
      inviter: { id: '1', acct: 'alice', display_name: 'Alice' },
    } }))
    await page.goto('/signup?invite=valid')
    await expect(page.getByText('Invited by @alice.', { exact: false })).toBeVisible()
    await expect(page.getByLabel('Why do you want to join?')).toHaveCount(bypass ? 0 : 1)
    await expect(page.locator('form').getByRole('button', { name: bypass ? 'Create account' : 'Apply for an account', exact: true })).toBeVisible()
  })
}

test('expired invite is explained before submitting signup', async ({ page }) => {
  await page.route('**/api/v2/instance', r => r.fulfill({ json: instance }))
  await page.route('**/api/eunha/v1/invite?*', r => r.fulfill({ json: { valid: false, reason: 'err_invite_expired' } }))
  await page.goto('/signup?invite=expired')
  await expect(page.getByText('This invite has expired.')).toBeVisible()
  await page.getByRole('checkbox').check()
  await expect(page.getByRole('button', { name: 'Apply for an account' })).toBeDisabled()
})

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

const invite = { id: '10', code: 'abc', url: 'https://example.invalid/signup?invite=abc',
  uses: 0, max_uses: 1, expires_at: null, expired: false, valid_for_use: true,
  bypass_approval: true, grant: null, autofollow: false, comment: null, created_at: '2026-01-01T00:00:00Z' }
const member = { id: '1', username: 'alice', acct: 'alice', display_name: 'Alice', avatar: '',
  invited_at: '2026-01-01T00:00:00Z', children: [] }

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
  await page.route('**/api/eunha/v1/invite_grants', r => { submitted = r.request().postDataJSON(); return r.fulfill({ json: { granted: 1, accounts: 1 } }) })
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
  await expect.poll(() => submitted).toEqual({ count: 2, max_uses: 1, expires_in: 604800 })

})

test('grant member lookup failure disables grants and offers retry', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
  let failed = true
  await page.route('**/api/eunha/v1/invite_grants/recipients', r => failed ? r.fulfill({ status: 500 }) : r.fulfill({ json: [member] }))
  await page.route('**/api/v1/admin/invites**', r => r.fulfill({ json: [] }))
  await page.goto('/admin/invites?grant_to=1')
  await expect(page.getByText('Could not load members.', { exact: false })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Review grant', exact: true })).toBeDisabled()
  failed = false
  await page.getByRole('button', { name: 'Try again' }).click()
  await expect(page.getByRole('button', { name: 'Review grant', exact: true })).toBeEnabled()
})

test('admin invite list exposes URLs and respects validity', async ({ page }) => {
  await signIn(page)
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

const tree = { total: 5, roots: [{ ...member, id: '0', acct: 'founder', username: 'founder', display_name: 'Founder',
  root_reason: 'no_recorded_inviter', children: [{ ...member, children: [
    { ...member, id: '2', acct: 'bob', username: 'bob', display_name: 'Bob', children: [
      { ...member, id: '4', acct: 'carol', username: 'carol', display_name: 'Carol' },
    ] },
  ] }, { ...member, id: '3', acct: 'sibling', username: 'sibling', display_name: 'Sibling' }] }] }

test('tree starts with my ancestry and supports collapse, search and finding me', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/eunha/v1/invite_tree', r => r.fulfill({ json: tree }))
  await page.goto('/invite-tree')
  const lineage = page.getByRole('list', { name: 'Invite lineage' })
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
  await expect(page.getByRole('button', { name: 'Whole instance' })).toHaveAttribute('aria-pressed', 'true')
  await expect(lineage.getByRole('link', { name: 'Sibling @sibling' })).toBeVisible()
  await lineage.getByRole('link', { name: 'Hand out invites to @alice' }).click()
  await expect(page).toHaveURL(/\/admin\/invites\?grant_to=1$/)
})

test('tree explains unavailable ancestry and keeps a deep search usable on mobile', async ({ page }) => {
  await signIn(page, 1 << 16)
  await page.setViewportSize({ width: 390, height: 844 })
  let chain = { ...member, id: '20', acct: 'last', username: 'last', display_name: 'Last' }
  for (let i = 19; i >= 0; i--) chain = { ...member, id: String(i), acct: `member${i}`, username: `member${i}`, display_name: `Member ${i}`, children: [chain] }
  await page.route('**/api/eunha/v1/invite_tree', r => r.fulfill({ json: { total: 21, roots: [{ ...chain, root_reason: 'inviter_unavailable' }] } }))
  await page.goto('/invite-tree')
  await expect(page.getByText('Inviter is unavailable in this view', { exact: true })).toBeVisible()
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
  await page.route('**/api/eunha/v1/invite_tree', r => failed ? r.fulfill({ status: 500 }) : r.fulfill({ json: tree }))
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
})

test('available link expires while open and removes the sidebar count', async ({ page }) => {
  await signIn(page, 0)
  await page.clock.install({ time: new Date('2026-10-05T00:00:00Z') })
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [{ ...invite, expires_at: '2026-10-05T00:00:02Z' }] }))
  await page.goto('/invites')
  await expect(page.getByRole('heading', { name: 'Invite 1 person' })).toBeVisible()
  await page.clock.fastForward(3000)
  await expect(page.getByRole('heading', { name: 'No invites available' })).toBeVisible()
  await expect(page.locator('aside span[aria-label$="available single-use invite links"]')).toHaveCount(0)
})


test('failed grant can be retried from review without changing the recipient', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
  await page.route('**/api/v1/admin/invites**', r => r.fulfill({ json: [] }))
  await page.route('**/api/eunha/v1/invite_grants/recipients', r => r.fulfill({ json: [member] }))
  let failed = true
  await page.route('**/api/eunha/v1/invite_grants', r => failed ? r.fulfill({ status: 500 }) : r.fulfill({ json: { granted: 3, accounts: 1 } }))
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
