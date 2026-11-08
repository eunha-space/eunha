import { expect, test } from '@playwright/test'

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([
      { token: 'test-token', account: { id: '1', acct: 'alice' } },
    ]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/verify_credentials', (route) =>
    route.fulfill({ json: {
      id: '1', acct: 'alice', username: 'alice', display_name: 'Alice', emojis: [],
      source: { privacy: 'public', language: 'en' },
    } }),
  )
})

test('default post language is saved to the account credentials', async ({ page }) => {
  await page.route('**/api/eunha/v1/preferences/languages', (route) =>
    route.fulfill({ json: [['en', 'English', 'English'], ['ko', 'Korean', '한국어']] }),
  )
  await page.route('**/api/v1/accounts/update_credentials', (route) => {
    expect(route.request().postData()).toMatch(/name="source\[language\]"\r\n\r\nko/)
    return route.fulfill({ json: { source: { language: 'ko' } } })
  })
  await page.goto('/settings')
  const picker = page.getByRole('button', { name: 'Default post language' })
  await expect(picker).toHaveText('English')
  await picker.click()
  await page.getByRole('button', { name: 'Korean · 한국어' }).click()
  await expect(picker).toHaveText('Korean')
})

test('a keyword filter is created with the selected contexts and action', async ({ page }) => {
  let saved: Record<string, unknown> | null = null
  await page.route('**/api/v2/filters', (route) => {
    if (route.request().method() === 'POST') {
      saved = route.request().postDataJSON()
      return route.fulfill({ json: { id: '7' } })
    }
    return route.fulfill({ json: saved ? [{
      id: '7', title: 'Spoilers', context: ['home'], filter_action: 'warn',
      keywords: [{ id: '8', keyword: 'ending', whole_word: true }], statuses: [],
    }] : [] })
  })
  await page.goto('/filters')
  await page.getByRole('button', { name: 'New filter' }).click()
  await page.getByRole('textbox', { name: 'Filter name' }).fill('Spoilers')
  await page.getByRole('textbox', { name: 'Keyword 1' }).fill('ending')
  await page.getByRole('button', { name: 'Save filter' }).click()
  await expect(page.getByRole('heading', { name: 'Spoilers' })).toBeVisible()
  expect(saved).toMatchObject({
    title: 'Spoilers', context: ['home'], filter_action: 'warn',
    keywords_attributes: [{ keyword: 'ending', whole_word: true }],
  })
})

test('creating a list opens its timeline and allows editing list preferences', async ({ page }) => {
  let changes: Record<string, unknown> | null = null
  const list = { id: '7', title: 'Friends', replies_policy: 'list', exclusive: false }
  await page.route('**/api/v1/lists', (route) =>
    route.fulfill({ json: route.request().method() === 'POST' ? list : [] }),
  )
  await page.route('**/api/v1/lists/7', (route) => {
    if (route.request().method() !== 'GET') changes = route.request().postDataJSON()
    return route.fulfill({ json: { ...list, ...changes } })
  })
  await page.route('**/api/v1/lists/7/accounts?*', (route) => route.fulfill({ json: [] }))
  await page.route('**/api/v1/timelines/list/7?*', (route) => route.fulfill({ json: [] }))
  await page.goto('/lists')
  await page.getByRole('textbox', { name: 'List name' }).fill('Friends')
  await page.getByRole('button', { name: 'Create list' }).click()
  await expect(page).toHaveURL(/\/lists\/7$/)
  await page.getByRole('button', { name: 'Manage list' }).click()
  await page.getByRole('switch', { name: 'Hide list members from Home' }).click()
  await page.getByRole('button', { name: 'Save list' }).click()
  await expect.poll(() => changes).toMatchObject({ exclusive: true, replies_policy: 'list' })
})
