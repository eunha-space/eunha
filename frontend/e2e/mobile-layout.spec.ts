import { expect, test } from '@playwright/test'

// Phone-sized. Nothing here needs a backend: the layout is chosen from
// localStorage before the first request, and every feed below will fail to
// load — which is not what any of this asserts.
const signedInWithAdvancedOn = async (page: import('@playwright/test').Page) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'e2e-layout-only', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
    localStorage.setItem('eunha:panes', 'on')
  })
}

test.describe('on a phone', () => {
  test.use({ viewport: { width: 375, height: 812 } })

  test('the advanced layout gives way to the single column', async ({ page }) => {
    await signedInWithAdvancedOn(page)
    await page.goto('/')

    // Not merely hidden: a row of 24rem panes has no reading of a 375px screen
    // worth rendering, and three panes would each mount a feed of their own.
    await expect(page.locator('.column-frame')).toBeVisible()
    await expect(page.locator('.advanced-pane')).toHaveCount(0)
    await expect(page.locator('.advanced-frame')).toHaveCount(0)
  })

  test('the timeline runs edge to edge', async ({ page }) => {
    await page.goto('/local')
    const frame = page.locator('.column-frame')
    await expect(frame).toBeVisible()

    // The card's border landed on the bezel and its corners clipped content
    // for nothing, so below `md` there is no card.
    await expect(frame).toHaveCSS('border-top-width', '0px')
    await expect(frame).toHaveCSS('border-left-width', '0px')
    await expect(frame).toHaveCSS('border-top-left-radius', '0px')

    const width = await page.evaluate(() => ({
      frame: document.querySelector('.column-frame')!.getBoundingClientRect().width,
      viewport: window.innerWidth,
      scroll: document.documentElement.scrollWidth,
    }))
    expect(width.frame).toBe(width.viewport)
    // Full bleed and no sideways scroll are two different things, and the
    // second is the one a stray margin breaks.
    expect(width.scroll).toBe(width.viewport)
  })

  // The menu button used to sit at -4px on a column page — half of it off the
  // screen — because `TopBar` is mounted outside the frame there and inside it
  // on a `.page-frame` page. Both are checked: the fix is a shared margin, so
  // one passing says nothing about the other.
  for (const [path, contentSelector] of [
    ['/local', '.column-frame header button'],
    ['/about', '.page-frame h1'],
  ] as const) {
    test(`the menu button sits on the content margin (${path})`, async ({ page }) => {
      await page.goto(path)
      const header = page.locator('header.mobile-header')
      await expect(header).toBeVisible()

      const left = await page.evaluate((selector) => {
        const trigger = document.querySelector('header.mobile-header button')!
        const content = document.querySelector(selector)
        return {
          trigger: trigger.getBoundingClientRect().left,
          content: content ? content.getBoundingClientRect().left : null,
          headerLeft: document
            .querySelector('header.mobile-header')!
            .getBoundingClientRect().left,
          headerWidth: document
            .querySelector('header.mobile-header')!
            .getBoundingClientRect().width,
          viewport: window.innerWidth,
        }
      }, contentSelector)

      expect(left.trigger).toBe(12)
      if (left.content !== null) expect(left.content).toBe(left.trigger)
      // The bar itself still reaches both edges, so its rule spans the screen.
      expect(left.headerLeft).toBe(0)
      expect(left.headerWidth).toBe(left.viewport)
    })
  }
})

test.describe('on a wide screen', () => {
  test.use({ viewport: { width: 1280, height: 900 } })

  test('the advanced layout is still the advanced layout', async ({ page }) => {
    await signedInWithAdvancedOn(page)
    await page.goto('/')
    await expect(page.locator('.advanced-frame')).toBeVisible()
    await expect(page.locator('.advanced-pane')).toHaveCount(3)
    await expect(page.locator('.column-frame')).toHaveCount(0)
  })

  test('the single column is still a card', async ({ page }) => {
    await page.addInitScript(() => localStorage.setItem('eunha:panes', 'off'))
    await page.goto('/local')
    const frame = page.locator('.column-frame')
    await expect(frame).toHaveCSS('border-top-width', '1px')
    await expect(frame).toHaveCSS('border-top-left-radius', '10px')
    await expect(page.locator('header.mobile-header')).toBeHidden()
  })
})
