import { expect, test } from '@playwright/test'

test('theme pill toggles dark mode', async ({ page }) => {
  await page.goto('/')
  const html = page.locator('html')
  const pill = page.getByRole('group', { name: 'Theme' })
  await expect(pill.getByRole('button', { name: 'System' })).toHaveAttribute('aria-pressed', 'true')

  // What the browser draws of its own accord — scrollbars, form controls — is
  // set by `color-scheme` and `scrollbar-color` on <html>, and on macOS, where
  // scrollbars are overlays, none of it can be seen. Only an assertion catches
  // it, so read both back rather than trusting the class alone.
  //
  // The thumb is compared against the theme's own `--scrollbar-thumb` rather
  // than a literal colour, so the tone can be tuned without touching the test:
  // what has to stay true is that the bar follows the theme. Both go through a
  // computed `scrollbar-color` so the two are normalised the same way.
  const chrome = () =>
    page.evaluate(() => {
      const root = document.documentElement
      const probe = document.createElement('div')
      probe.style.scrollbarColor = `${getComputedStyle(root).getPropertyValue(
        '--scrollbar-thumb',
      )} transparent`
      document.body.appendChild(probe)
      const fromToken = getComputedStyle(probe).scrollbarColor
      probe.remove()
      return {
        colorScheme: getComputedStyle(root).colorScheme,
        scrollbar: getComputedStyle(root).scrollbarColor,
        fromToken,
      }
    })

  // Switch to Dark.
  await pill.getByRole('button', { name: 'Dark' }).click()
  await expect(html).toHaveClass(/dark/)
  await expect(pill.getByRole('button', { name: 'Dark' })).toHaveAttribute('aria-pressed', 'true')
  const dark = await chrome()
  expect(dark.colorScheme).toBe('dark')
  expect(dark.scrollbar).not.toBe('auto')
  expect(dark.scrollbar).toBe(dark.fromToken)

  // Switch back to Light.
  await pill.getByRole('button', { name: 'Light' }).click()
  await expect(html).not.toHaveClass(/dark/)
  const light = await chrome()
  expect(light.colorScheme).toBe('light')
  expect(light.scrollbar).not.toBe('auto')
  expect(light.scrollbar).toBe(light.fromToken)

  // And the two themes really do paint different bars — a token that stopped
  // being overridden would satisfy every assertion above.
  expect(light.scrollbar).not.toBe(dark.scrollbar)
})

test('renders the header with a sign-in action when logged out', async ({
  page,
}) => {
  await page.goto('/')
  await expect(page.getByRole('button', { name: 'Sign in' })).toBeVisible()
})
