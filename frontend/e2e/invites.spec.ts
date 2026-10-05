import { expect, test } from '@playwright/test'

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
