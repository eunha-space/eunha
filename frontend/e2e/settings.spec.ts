import { expect, test } from '@playwright/test'

// Client-only: the page renders its signed-in branch from localStorage, and the
// one request it makes (DELETE /api/v1/accounts) is stubbed here, so no backend
// is needed.
const signedIn = async (page: import('@playwright/test').Page) => {
  await page.addInitScript(() => {
    if (sessionStorage.getItem('settings-test-login')) return
    sessionStorage.setItem('settings-test-login', '1')
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
    localStorage.setItem(
      'eunha:me-account',
      JSON.stringify({
        id: '1',
        acct: 'alice',
        defaultVisibility: 'public',
      }),
    )
  })
}

test('settings asks to sign in when logged out', async ({ page }) => {
  await page.goto('/settings')

  await expect(
    page.getByText('Sign in to manage your account.'),
  ).toBeVisible()
  await expect(
    page.getByRole('heading', { name: 'Delete account' }),
  ).toBeHidden()
})

test('deleting an account takes the username, a password and a confirmation', async ({
  page,
}) => {
  await signedIn(page)

  let deleteBody: string | null = null
  await page.route('**/api/v1/accounts', async (route) => {
    deleteBody = route.request().postData()
    await route.fulfill({ status: 200, body: '' })
  })

  await page.goto('/settings')
  await expect(
    page.getByRole('heading', { name: 'Delete account' }),
  ).toBeVisible()
  await expect(
    page.getByText('You will not be able to restore or reactivate your account'),
  ).toBeVisible()

  // The submit button stays disabled until the challenge is filled in.
  // (Scoped to the form: the confirmation dialog carries the same label.)
  const submit = page
    .locator('form')
    .getByRole('button', { name: 'Delete account' })
  await expect(submit).toBeDisabled()
  await page.getByLabel('Current password').fill('hunter2hunter2')
  // Since Mastodon 4.7.3 the username is asked for as well.
  await expect(submit).toBeDisabled()
  await expect(page.locator('form code')).toHaveText('alice')
  await page.getByLabel('Confirm username').fill('alice')
  await expect(submit).toBeEnabled()

  // Submitting only opens the confirmation; cancelling sends nothing.
  await submit.click()
  await expect(
    page.getByRole('alertdialog', { name: 'Delete your account?' }),
  ).toBeVisible()
  await page.getByRole('button', { name: 'Cancel' }).click()
  expect(deleteBody).toBeNull()

  // Confirming sends the challenge and drops the local session.
  await submit.click()
  await page
    .getByRole('alertdialog')
    .getByRole('button', { name: 'Delete account' })
    .click()

  expect(deleteBody).toBe(
    JSON.stringify({ username: 'alice', password: 'hunter2hunter2' }),
  )
  await expect(page).toHaveURL(/\/$/)
  expect(await page.evaluate(() => localStorage.getItem('eunha:active-account'))).toBeNull()
})
