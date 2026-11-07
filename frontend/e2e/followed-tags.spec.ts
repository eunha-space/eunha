import { expect, test } from '@playwright/test'

test('followed hashtags follow the server cursor and can be removed from the list', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([
      { token: 'test-token', account: { id: '1', acct: 'alice' } },
    ]))
    localStorage.setItem('eunha:active-account', '1')
  })
  const tag = (name: string) => ({
    name, url: `https://example.invalid/tags/${name}`, following: true, history: [],
  })
  await page.route('**/api/v1/followed_tags?*', (route) => {
    const url = new URL(route.request().url())
    if (url.searchParams.get('max_id') === '12345') {
      return route.fulfill({ json: [tag('한글')] })
    }
    url.searchParams.set('max_id', '12345')
    return route.fulfill({
      json: [tag('eunha')],
      headers: { Link: `<${url}>; rel="next"` },
    })
  })
  await page.route('**/api/v1/tags/eunha/unfollow', (route) =>
    route.fulfill({ json: { ...tag('eunha'), following: false } }),
  )

  await page.goto('/')
  await page.getByRole('link', { name: 'Followed hashtags', exact: true }).click()
  await expect(page).toHaveURL(/\/followed_tags$/)
  await expect(page.getByRole('link', { name: '#eunha' })).toHaveAttribute('href', '/tags/eunha')
  await page.getByRole('button', { name: 'Unfollow #eunha' }).click()
  await expect(page.getByRole('link', { name: '#eunha' })).toHaveCount(0)
  await page.getByRole('button', { name: 'Load more' }).click()
  await expect(page.getByRole('link', { name: '#한글' })).toHaveAttribute('href', '/tags/%ED%95%9C%EA%B8%80')
})
