import { expect, test } from '@playwright/test'

const account = { id: '1', username: 'carol', acct: 'carol@remote.example', display_name: 'Carol' }

async function signIn(page: import('@playwright/test').Page) {
  await page.addInitScript((me) => {
    localStorage.setItem('eunha:active-account', me.id)
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 't', account: me }]))
  }, { id: '9', username: 'alice', acct: 'alice' })
}

test('the remote follow page shows the account it was sent to', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v2/search**', (r) =>
    r.fulfill({ json: { accounts: [account], statuses: [], hashtags: [] } }),
  )

  await page.goto('/authorize_interaction?uri=carol%40remote.example')
  await expect(page).toHaveURL(/\/@carol@remote\.example$/)
})

test('the remote follow page says so when it finds nothing', async ({ page }) => {
  await signIn(page)
  await page.route('**/api/v2/search**', (r) =>
    r.fulfill({ json: { accounts: [], statuses: [], hashtags: [] } }),
  )

  await page.goto('/authorize_interaction?uri=https%3A%2F%2Fremote.example%2Fnope')
  await expect(page.getByText('Nothing was found at that address.')).toBeVisible()
})

test('sharing opens the composer with what was shared', async ({ page }) => {
  await signIn(page)

  await page.goto('/share?title=A%20title&text=Some%20text&url=https%3A%2F%2Fexample.com%2F')
  await expect(page.getByRole('textbox').first()).toHaveValue(
    'A title Some text https://example.com/',
  )
})
