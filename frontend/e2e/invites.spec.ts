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
  autofollow: false, comment: 'Friends', created_at: '2026-01-01T00:00:00Z' }
const member = { id: '1', username: 'alice', acct: 'alice', display_name: 'Alice', avatar: '',
  invited_at: '2026-01-01T00:00:00Z', children: [] }

test('personal invites retain revoked history and distinguish fully used links', async ({ page }) => {
  await signIn(page, 1 << 16)
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [invite, { ...invite, id: '11', uses: 1, valid_for_use: false }] }))
  await page.route('**/api/v1/invites/10', r => r.fulfill({ status: 204 }))
  await page.goto('/invites')
  await expect(page.getByText('Fully used', { exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Revoke invite' })).toHaveCount(1)
  await page.getByRole('button', { name: 'Revoke invite' }).click()
  await expect(page.getByText('Expired', { exact: true })).toBeVisible()
  await expect(page.getByLabel('Invite link')).toHaveCount(2)
  await expect(page.getByRole('button', { name: 'Revoke invite' })).toHaveCount(0)
})

test('grant recipients are explicit and bulk grants show their reach', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
  await page.route('**/api/eunha/v1/invite_tree', r => r.fulfill({ json: { roots: [member], total: 1 } }))
  let submitted: unknown
  await page.route('**/api/eunha/v1/invite_grants', r => { submitted = r.request().postDataJSON(); return r.fulfill({ json: { granted: 1, accounts: 1 } }) })
  await page.goto('/invites')
  const grant = page.getByRole('button', { name: 'Hand out invites', exact: true })
  await expect(grant).toBeDisabled()
  await page.getByLabel('Search members').fill('alice')
  await page.getByRole('combobox', { name: 'Recipient' }).click()
  await page.getByRole('option', { name: 'Everyone (1 member)' }).click()
  await expect(page.getByText('Create 1 link for 1 member; each link admits 1 person.')).toBeVisible()
  await grant.click()
  await expect.poll(() => submitted).toEqual({ count: 1, max_uses: 1 })
})

test('grant member lookup failure disables grants and offers retry', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
  let failed = true
  await page.route('**/api/eunha/v1/invite_tree', r => failed ? r.fulfill({ status: 500 }) : r.fulfill({ json: { roots: [member], total: 1 } }))
  await page.goto('/invites?grant_to=1')
  await expect(page.getByText('Could not load members.', { exact: false })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Hand out invites', exact: true })).toBeDisabled()
  failed = false
  await page.getByRole('button', { name: 'Try again' }).click()
  await expect(page.getByRole('button', { name: 'Hand out invites', exact: true })).toBeEnabled()
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
