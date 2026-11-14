import { expect, test } from '@playwright/test'

// The server writes a profile's own name first in `<title>`, and names the
// site in `application-name`. The rail must take the site's name from the
// latter while `/api/v2/instance` is still out, not show the profile's.
test('the rail names the server, not the profile, before the instance loads', async ({ page }) => {
  await page.route(/\/@alice$/, async route => {
    const response = await route.fetch()
    const html = (await response.text())
      .replace('<title>eunha</title>', '<title>Alice (@alice@example.invalid) - Example</title>')
      .replace('</head>', '<meta name="application-name" content="Example" /></head>')
    await route.fulfill({ response, body: html })
  })
  let release!: () => void
  const held = new Promise<void>(resolve => { release = resolve })
  await page.route('**/api/v2/instance', async r => {
    await held
    await r.fulfill({ json: { title: 'Example', domain: 'example.invalid', registrations: { enabled: false } } })
  })
  await page.route('**/api/v1/accounts/lookup**', r => r.fulfill({ status: 404, json: { error: 'Record not found' } }))

  await page.goto('/@alice')
  const rail = page.locator('aside')
  await expect(rail.getByText('Example', { exact: true })).toBeVisible()
  await expect(rail.getByText(/Alice/)).toHaveCount(0)
  release()
  await expect(rail.getByText('Example', { exact: true })).toBeVisible()
})
