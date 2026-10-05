import { expect, test } from '@playwright/test'

const me = {
  id: '1',
  username: 'alice',
  acct: 'alice',
  display_name: 'Alice',
  avatar: '',
  source: { privacy: 'public' },
}

async function signedIn(page: import('@playwright/test').Page) {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/verify_credentials**', (r) => r.fulfill({ json: me }))
  await page.route('**/api/v1/notifications/unread_count**', (r) =>
    r.fulfill({ json: { count: 0 } }),
  )
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/notifications**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/conversations**', (r) => r.fulfill({ json: [] }))
}

// Picking a column up and carrying it somewhere. Two moves rather than one:
// the press has to travel a few pixels before it counts as a drag at all —
// that is what leaves the title its click — and only what follows is carried
// over anything.
async function dragColumn(
  page: import('@playwright/test').Page,
  from: number,
  to: number,
) {
  const bar = (await page.locator('.advanced-pane > header').nth(from).boundingBox())!
  const target = (await page.locator('.advanced-pane').nth(to).boundingBox())!
  await page.mouse.move(bar.x + 40, bar.y + 3)
  await page.mouse.down()
  await page.mouse.move(bar.x + 60, bar.y + 3)
  await page.mouse.move(target.x + target.width / 2, bar.y + 3)
  await page.mouse.up()
}

// The nav says Home; the column says what the feed is. Both are deliberate,
// and the accessible label stays "Home" so the two never disagree for a screen
// reader.
test('the home column is headed Following', async ({ page }) => {
  await signedIn(page)
  await page.goto('/')

  const column = page.locator('.column-frame')
  await expect(column.getByRole('button', { name: 'Following' })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Home' })).toBeVisible()
  // And the rail still calls it Home.
  await expect(page.locator('aside').getByRole('link', { name: 'Home' })).toBeVisible()
})

test('local and federated columns are headed by their feed', async ({ page }) => {
  await signedIn(page)
  await page.goto('/local')
  await expect(
    page.locator('.column-frame').getByRole('button', { name: 'Local' }),
  ).toBeVisible()

  await page.goto('/public')
  await expect(
    page.locator('.column-frame').getByRole('button', { name: 'Federated' }),
  ).toBeVisible()
})

// The advanced layout is off unless asked for, which is the whole point of
// gating it — the default stays one column.
test('the advanced layout is opt-in and remembers its panes', async ({ page }) => {
  await signedIn(page)
  await page.goto('/')
  await expect(page.locator('.advanced-pane')).toHaveCount(0)

  await page.goto('/settings')
  await page.getByRole('switch').first().click()

  await page.goto('/')
  await expect(page.locator('.advanced-pane')).toHaveCount(3)

  // Closing one sticks across a reload, because it is stored, not just state.
  await page.getByRole('button', { name: 'Close Local' }).click()
  await expect(page.locator('.advanced-pane')).toHaveCount(2)
  await page.reload()
  await expect(page.locator('.advanced-pane')).toHaveCount(2)

  // Only what is missing is offered back.
  await page.getByRole('button', { name: 'Add a timeline' }).click()
  await expect(page.getByRole('menuitem')).toContainText(['Local'])
})

// It opens on the two panes a second column is *for* — the feed and what is
// addressed to you — plus one more to choose.
test('the advanced layout opens on timeline, notifications and one more', async ({
  page,
}) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  const titles = page.locator('.advanced-pane > header button').first()
  await expect(titles).toHaveText('Following')
  await expect(page.locator('.advanced-pane')).toHaveCount(3)
  await expect(
    page
      .locator('.advanced-pane')
      .nth(1)
      .getByRole('button', { name: 'Notifications', exact: true }),
  ).toBeVisible()
})

// The rail's default `left` follows a centred reading column, which this
// layout does not have — unpinned, it lands on top of the first pane on any
// wide screen. Checked at a width where the old positioning overlapped by
// 200px.
test('the rail does not overlap the first pane on a wide screen', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()

  for (const width of [1280, 1600, 2200]) {
    await page.setViewportSize({ width, height: 900 })
    await page.goto('/')
    const rail = await page.locator('aside.sidebar-frame').boundingBox()
    const panes = await page.locator('.advanced-frame').boundingBox()
    expect(rail, `rail missing at ${width}`).not.toBeNull()
    expect(panes, `panes missing at ${width}`).not.toBeNull()
    expect(rail!.x + rail!.width, `overlap at ${width}px`).toBeLessThanOrEqual(panes!.x)
  }
})

// The rail is fixed, so it cannot be centred by a flow it is not in. Rail and
// panes are placed together from one measured number: centred while the group
// fits, pinned once it does not.
test('the rail and panes are centred together while they fit', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()

  const edges = async () => {
    const rail = await page.locator('aside.sidebar-frame').boundingBox()
    const gap = await page.evaluate(() => {
      const row = document.querySelector('.advanced-frame')
      if (!row) return null
      const style = getComputedStyle(row)
      const pad =
        (parseFloat(style.paddingLeft) || 0) + (parseFloat(style.paddingRight) || 0)
      const kids = Array.from(row.children) as HTMLElement[]
      const content =
        kids.reduce((sum, k) => sum + k.offsetWidth, 0) +
        (parseFloat(style.columnGap) || 0) * (kids.length - 1) +
        pad
      const left = row.getBoundingClientRect().left
      return window.innerWidth - (left + content)
    })
    return { left: rail!.x, right: gap! }
  }

  // Wide enough for rail plus three panes: even margins either side.
  await page.setViewportSize({ width: 2000, height: 900 })
  await page.goto('/')
  let e = await edges()
  expect(Math.abs(e.left - e.right), 'not centred at 2000px').toBeLessThan(4)
  expect(e.left, 'centred group should not be pinned').toBeGreaterThan(20)

  // Too narrow for the group: pinned left, and the panes scroll instead.
  await page.setViewportSize({ width: 1300, height: 900 })
  await page.goto('/')
  e = await edges()
  expect(Math.round(e.left), 'should pin once it stops fitting').toBe(16)

  // And it follows a resize, not just a fresh load.
  await page.setViewportSize({ width: 2000, height: 900 })
  await expect
    .poll(async () => {
      const after = await edges()
      return Math.abs(after.left - after.right) < 4
    })
    .toBe(true)
})

// A layout showing nothing is not a state worth being able to reach, so the
// last pane's close button goes dead rather than emptying the frame.
test('the last pane cannot be closed', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  await page.getByRole('button', { name: 'Close Local' }).click()
  await page.getByRole('button', { name: 'Close Notifications' }).click()
  await expect(page.locator('.advanced-pane')).toHaveCount(1)

  const close = page.getByRole('button', { name: 'Close Following' })
  await expect(close).toBeDisabled()
  await close.click({ force: true })
  await expect(page.locator('.advanced-pane')).toHaveCount(1)
})

// The order of the columns is the reader's, not ours. Dragging a bar is how a
// pointer says so.
test('a column can be dragged into a new place, and it sticks', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  const titles = page.locator('.advanced-pane > header > button')
  await expect(titles).toHaveText(['Following', 'Notifications', 'Local'])

  // Local, from the far end, onto Following at the near one.
  await dragColumn(page, 2, 0)
  await expect(titles).toHaveText(['Local', 'Following', 'Notifications'])

  // Stored, not just state: an order that forgets itself on reload is not an
  // order anybody would arrange.
  await page.reload()
  await expect(titles).toHaveText(['Local', 'Following', 'Notifications'])

  // And the whole row in one pass, which is the case that would land short of
  // where it was dropped if the order were settled from the last thing the
  // pointer happened to be over.
  await dragColumn(page, 0, 2)
  await expect(titles).toHaveText(['Following', 'Notifications', 'Local'])
})

// The bar is the grip, all of it: the title, the space either side of it, the
// padding above and below. Only the controls at the end are not, because
// pressing Close is not a way to start moving a column.
test('the whole bar is the grip, except the close button', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  const titles = page.locator('.advanced-pane > header > button')

  // `dragColumn` presses the bar 3px from its top edge, above the title and
  // clear of every control in it.
  await dragColumn(page, 0, 1)
  await expect(titles).toHaveText(['Notifications', 'Following', 'Local'])

  // The close button moves nothing — nor does the refused drag leave a click
  // behind that closes the column.
  const close = page.getByRole('button', { name: 'Close Notifications' })
  const cb = (await close.boundingBox())!
  await page.mouse.move(cb.x + cb.width / 2, cb.y + cb.height / 2)
  await page.mouse.down()
  await page.mouse.move(cb.x + 100, cb.y)
  await page.mouse.move(cb.x + 200, cb.y)
  await page.mouse.up()
  await expect(titles).toHaveText(['Notifications', 'Following', 'Local'])
  await expect(page.locator('.advanced-pane')).toHaveCount(3)
})

// The bar says it can be picked up by the cursor it shows, and the title is
// the widest part of it. A button carries a cursor of its own, so left alone
// the grip looks like it stops at the padding around the words.
test('the whole bar shows a cursor that says it can be dragged', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  const cursors = await page.evaluate(() => {
    const bar = document.querySelector('.advanced-pane > header')!
    const buttons = bar.querySelectorAll('button')
    return {
      bar: getComputedStyle(bar).cursor,
      title: getComputedStyle(buttons[0]).cursor,
      close: getComputedStyle(buttons[1]).cursor,
    }
  })
  expect(cursors.bar).toBe('grab')
  expect(cursors.title).toBe('grab')
  // And the close button does not claim to be a grip, because it is not one.
  expect(cursors.close).not.toBe('grab')
})

// The bar being a handle must not cost the title its click: pressing on the
// handle is where a drag begins, and a press that never moves is still a
// click that scrolls the column back to the top.
test('the title still scrolls its column to the top', async ({ page }) => {
  await signedIn(page)
  await page.addInitScript(() => {
    ;(window as never as { __scrolled: number }).__scrolled = 0
    window.scrollTo = () => {
      ;(window as never as { __scrolled: number }).__scrolled++
    }
  })
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  await page.locator('.advanced-pane > header > button').first().click()
  expect(
    await page.evaluate(() => (window as never as { __scrolled: number }).__scrolled),
  ).toBe(1)
  await expect(page.locator('.advanced-pane > header > button')).toHaveText([
    'Following',
    'Notifications',
    'Local',
  ])
})

// Creeping across a boundary a few pixels at a time settles on one order
// rather than swapping back and forth across it. This is what the hand-rolled
// version got wrong — it asked what was under the cursor, and a column sliding
// into its new place is drawn where it used to be for as long as the slide
// lasts — so it is worth keeping asked of whatever does the dragging.
test('creeping across a boundary settles on one order', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  const header = page.locator('.advanced-pane > header').first()
  const hb = (await header.boundingBox())!
  const second = (await page.locator('.advanced-pane').nth(1).boundingBox())!

  await page.mouse.move(hb.x + 40, hb.y + 3)
  await page.mouse.down()
  for (let x = hb.x + 60; x < second.x + second.width - 20; x += 12) {
    await page.mouse.move(x, hb.y + 3)
  }
  await page.mouse.up()

  const titles = page.locator('.advanced-pane > header > button')
  await expect(titles).toHaveText(['Notifications', 'Following', 'Local'])
})

// What follows the cursor is the column itself: dnd-kit lifts this very
// element and leaves a placeholder holding its place in the row, rather than
// drawing a copy. A copy would be a second live timeline for as long as the
// drag lasted.
test('the column itself is what is dragged', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  const first = page.locator('.advanced-pane').first()
  const hb = (await first.locator('header').boundingBox())!
  const second = (await page.locator('.advanced-pane').nth(1).boundingBox())!

  await page.mouse.move(hb.x + 40, hb.y + 3)
  await page.mouse.down()
  await page.mouse.move(hb.x + 120, hb.y + 3)
  await expect(first).toHaveAttribute('data-dnd-dragging', 'true')
  await expect(page.locator('[data-dnd-placeholder]')).toHaveCount(1)

  await page.mouse.move(second.x + second.width / 2, hb.y + 3)
  await page.mouse.up()
  await expect(first).not.toHaveAttribute('data-dnd-dragging', 'true')
})

// A column that moves aside has to be seen doing it, or the row simply differs
// from one frame to the next and the reader has to work out what changed.
test('columns slide into their new places', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  const titles = page.locator('.advanced-pane > header > button')
  const header = page.locator('.advanced-pane > header').first()
  const hb = (await header.boundingBox())!
  const second = (await page.locator('.advanced-pane').nth(1).boundingBox())!

  await page.mouse.move(hb.x + 40, hb.y + 3)
  await page.mouse.down()
  await page.mouse.move(hb.x + 120, hb.y + 3)
  await page.mouse.move(second.x + second.width / 2, hb.y + 3)

  // The one shoved aside is animating, not simply redrawn somewhere else.
  const animating = await page.evaluate(() =>
    [...document.querySelectorAll('.advanced-pane')].filter(
      (pane) => pane.getAnimations().length > 0,
    ).length,
  )
  await page.mouse.up()
  expect(animating).toBeGreaterThan(0)
  await expect(titles).toHaveText(['Notifications', 'Following', 'Local'])
})

// Escape abandons a drag in progress, and the row goes back to the order it
// was picked up from. Letting go anywhere else keeps what is on screen: the
// columns move aside as the pointer passes, so what you see when you release
// is what you get, whether or not you release over the row.
test('escape abandons a drag; letting go keeps what is shown', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  const titles = page.locator('.advanced-pane > header > button')
  const hb = (await page.locator('.advanced-pane > header').first().boundingBox())!
  const last = (await page.locator('.advanced-pane').nth(2).boundingBox())!

  await page.mouse.move(hb.x + 40, hb.y + 3)
  await page.mouse.down()
  await page.mouse.move(hb.x + 60, hb.y + 3)
  await page.mouse.move(last.x + last.width / 2, hb.y + 3)
  await page.keyboard.press('Escape')
  await page.mouse.up()
  await expect(titles).toHaveText(['Following', 'Notifications', 'Local'])

  // Released below the row rather than on it, having passed over the third
  // column: the order it was left in is the order it keeps.
  await page.mouse.move(hb.x + 40, hb.y + 3)
  await page.mouse.down()
  await page.mouse.move(hb.x + 60, hb.y + 3)
  await page.mouse.move(last.x + last.width / 2, hb.y + 3)
  await page.mouse.move(last.x + last.width / 2, last.y + last.height + 60)
  await page.mouse.up()
  await expect(titles).toHaveText(['Notifications', 'Local', 'Following'])
  await page.reload()
  await expect(titles).toHaveText(['Notifications', 'Local', 'Following'])
})

// Dragging is the only move a mouse offers and the one nothing else can make,
// so the bar is a handle for the keyboard too: pick the column up, move it,
// put it down — the pattern the library announces to screen readers, rather
// than one of our own.
test('a column can be moved with the keyboard', async ({ page }) => {
  await signedIn(page)
  await page.goto('/settings')
  await page.getByRole('switch').first().click()
  await page.goto('/')

  const titles = page.locator('.advanced-pane > header > button')
  await page.locator('.advanced-pane > header').first().focus()
  await page.keyboard.press('Space')
  await page.keyboard.press('ArrowRight')
  await page.keyboard.press('Space')
  await expect(titles).toHaveText(['Notifications', 'Following', 'Local'])

  // And Escape puts it back rather than leaving it where it had got to.
  await page.locator('.advanced-pane > header').nth(1).focus()
  await page.keyboard.press('Space')
  await page.keyboard.press('ArrowRight')
  await page.keyboard.press('Escape')
  await expect(titles).toHaveText(['Notifications', 'Following', 'Local'])
})
