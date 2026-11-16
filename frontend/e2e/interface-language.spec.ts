import { expect, test } from '@playwright/test'

const alice = { id: '1', acct: 'alice', displayName: 'Alice', avatar: '', defaultVisibility: 'public' }

const preferences = (locale: string | null) => ({
  noindex: false,
  show_application: true,
  chosen_languages: null,
  locale,
  time_zone: null,
})

async function signIn(page: import('@playwright/test').Page, locale: string | null) {
  await page.addInitScript((alice) => {
    localStorage.setItem('eunha:active-account', '1')
    localStorage.setItem('eunha:me-account', JSON.stringify(alice))
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'alice-token', account: alice }]))
  }, alice)
  await page.route('**/api/v1/accounts/verify_credentials', (route) =>
    route.fulfill({
      json: { ...alice, username: alice.acct, display_name: alice.displayName, source: { privacy: 'public' } },
    }),
  )
  await page.route('**/api/v1/timelines/**', (route) => route.fulfill({ json: [] }))
  const saved: unknown[] = []
  await page.route('**/api/eunha/v1/preferences', (route) => {
    if (route.request().method() === 'PATCH') {
      const body = route.request().postDataJSON() as { locale: string }
      saved.push(body)
      return route.fulfill({ json: preferences(body.locale) })
    }
    return route.fulfill({ json: preferences(locale) })
  })
  return saved
}

test('the account menu switches the interface language and saves it to the account', async ({ page }) => {
  const saved = await signIn(page, null)
  await page.goto('/')
  const rail = page.locator('aside')
  await expect(rail.getByRole('link', { name: 'Home' })).toBeVisible()

  await page.getByRole('button', { name: 'Account menu' }).click()
  await page.getByRole('menuitem', { name: /Language/ }).click()
  await page.getByRole('menuitemradio', { name: '한국어' }).click()

  await expect(rail.getByRole('link', { name: '홈' })).toBeVisible()
  await expect(rail.getByRole('link', { name: '저장됨' })).toBeVisible()
  await expect(rail.getByRole('button', { name: '새 게시물' })).toBeVisible()
  await expect(page.locator('html')).toHaveAttribute('lang', 'ko')
  expect(saved).toEqual([{ locale: 'ko' }])

  // A reload paints in it straight away, before the preferences answer.
  await page.reload()
  await expect(rail.getByRole('link', { name: '홈' })).toBeVisible()
})

test("the account's language wins over the browser's", async ({ page }) => {
  await signIn(page, 'ko')
  await page.goto('/')
  await expect(page.locator('aside').getByRole('link', { name: '홈' })).toBeVisible()
})

test.describe('signed out', () => {
  test.use({ locale: 'ko-KR' })

  test("follows the browser's language, and the language button changes it", async ({ page }) => {
    await page.goto('/')
    const rail = page.locator('aside')
    await expect(rail.getByRole('link', { name: '둘러보기' })).toBeVisible()
    await expect(rail.getByRole('group', { name: '테마' })).toBeVisible()

    await page.getByRole('button', { name: '언어' }).click()
    await page.getByRole('menuitemradio', { name: 'English' }).click()
    await expect(rail.getByRole('link', { name: 'Explore' })).toBeVisible()
    await expect(page.locator('html')).toHaveAttribute('lang', 'en')

    await page.reload()
    await expect(rail.getByRole('link', { name: 'Explore' })).toBeVisible()
  })
})
