import { expect, test } from '@playwright/test'

const alice = { id: '1', acct: 'alice', displayName: 'Alice', avatar: '', defaultVisibility: 'public' }
const bob = { ...alice, id: '2', acct: 'bob', displayName: 'Bob' }

test('switching resets the profile and sign-out removes only the selected account', async ({ page }) => {
  await page.goto('/')
  await page.evaluate(({ alice, bob }) => {
    localStorage.setItem('eunha:token', 'alice-token')
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
  await expect.poll(() => page.evaluate(() => localStorage.getItem('eunha:token'))).toBe('bob-token')
  await expect(page.getByText('Bob', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Account menu' }).click()
  await page.getByRole('menuitem', { name: 'Sign out', exact: true }).click()
  await expect.poll(() => page.evaluate(() => localStorage.getItem('eunha:token'))).toBe('alice-token')
  await expect.poll(() => page.evaluate(() => JSON.parse(localStorage.getItem('eunha:accounts') ?? '[]').length)).toBe(1)
})
