import { expect, test } from '@playwright/test'

// Client-only: the account is signed in through localStorage, and
// `verify_credentials` and `update_credentials` are stubbed here.
const signedIn = async (page: import('@playwright/test').Page) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
}

const account = (discoverable: boolean | null) => ({
  id: '1',
  username: 'alice',
  acct: 'alice',
  display_name: '',
  note: '',
  avatar: '/avatars/original/missing.png',
  header: '/headers/original/missing.png',
  discoverable,
  locked: false,
  fields: [],
  emojis: [],
  source: { note: '', fields: [], privacy: 'public', sensitive: false, language: null },
})

const stub = async (page: import('@playwright/test').Page, discoverable: boolean | null) => {
  const sent: string[] = []
  await page.route('**/api/v1/accounts/verify_credentials', (route) =>
    route.fulfill({ json: account(discoverable) }),
  )
  await page.route('**/api/v1/accounts/update_credentials', async (route) => {
    sent.push(route.request().postData() ?? '')
    await route.fulfill({ json: account(true) })
  })
  return sent
}

test('the profile step starts discoverable and saves it with indexable', async ({ page }) => {
  await signedIn(page)
  const sent = await stub(page, null)

  await page.goto('/start/profile')
  await expect(page.getByRole('heading', { name: 'Profile setup' })).toBeVisible()
  const toggle = page.getByRole('switch', { name: /Make my profile discoverable/ })
  await expect(toggle).toBeChecked()

  await page.getByLabel('Display name').fill('Alice')
  await page.getByRole('button', { name: 'Finish' }).click()

  await expect(page).toHaveURL(/\/$/)
  expect(sent).toHaveLength(1)
  const body = sent[0]
  expect(body).toContain('Alice')
  expect(body).toMatch(/discoverable\W+true/)
  expect(body).toMatch(/indexable\W+true/)
})

test('the profile step keeps a choice already made', async ({ page }) => {
  await signedIn(page)
  const sent = await stub(page, false)

  await page.goto('/start/profile')
  const toggle = page.getByRole('switch', { name: /Make my profile discoverable/ })
  await expect(toggle).not.toBeChecked()

  await page.getByRole('button', { name: 'Finish' }).click()
  await expect(page).toHaveURL(/\/$/)
  expect(sent[0]).toMatch(/discoverable\W+false/)
})
