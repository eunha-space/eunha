import { expect, test } from '@playwright/test'

const alice = { id: '1', acct: 'alice', displayName: 'Alice', avatar: '', defaultVisibility: 'public' }
const bob = { ...alice, id: '2', acct: 'bob', displayName: 'Bob' }

test('switching resets the profile and sign-out removes only the selected account', async ({ page }) => {
  await page.goto('/')
  await page.evaluate(({ alice, bob }) => {
    localStorage.setItem('eunha:active-account', '1')
    localStorage.setItem('eunha:me-account', JSON.stringify(alice))
    localStorage.setItem('eunha:me-id', alice.id)
    localStorage.setItem('eunha:accounts', JSON.stringify([
      { token: 'alice-token', account: alice }, { token: 'bob-token', account: bob },
    ]))
  }, { alice, bob })
  await page.route('**/api/v1/accounts/verify_credentials', (route) => {
    const account = route.request().headers().authorization === 'Bearer bob-token' ? bob : alice
    return route.fulfill({ json: { ...account, username: account.acct, display_name: account.displayName, source: { privacy: 'public' } } })
  })
  await page.reload()
  await page.getByRole('button', { name: 'Account menu' }).click()
  await page.getByRole('menuitem', { name: '@bob', exact: true }).click()
  await expect.poll(() => page.evaluate(() => localStorage.getItem('eunha:active-account'))).toBe('2')
  await expect(page.getByText('Bob', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Account menu' }).click()
  await page.getByRole('menuitem', { name: 'Sign out', exact: true }).click()
  await expect.poll(() => page.evaluate(() => localStorage.getItem('eunha:active-account'))).toBe('1')
  await expect.poll(() => page.evaluate(() => JSON.parse(localStorage.getItem('eunha:accounts') ?? '[]').length)).toBe(1)
})

test('a legacy standalone token does not sign the browser in', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:token', 'legacy-token')
    localStorage.setItem('eunha:me-account', JSON.stringify({ id: '1', acct: 'alice', displayName: 'Alice' }))
  })
  await page.goto('/settings')
  await expect(page.getByRole('button', { name: 'Sign in' }).last()).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Delete account' })).toHaveCount(0)
  expect(await page.evaluate(() => localStorage.getItem('eunha:accounts'))).toBeNull()
})
