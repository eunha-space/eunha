import { expect, test } from '@playwright/test'

const account = (id: string, acct: string, name: string) => ({
  id,
  username: acct,
  acct,
  display_name: name,
  note: '',
  url: `https://example.invalid/@${acct}`,
  uri: `https://example.invalid/users/${acct}`,
  avatar: '',
  avatar_static: '',
  header: '',
  header_static: '',
  followers_count: 0,
  following_count: 0,
  statuses_count: 0,
  created_at: '2026-01-01T00:00:00.000Z',
  last_status_at: null,
  emojis: [],
  fields: [],
  locked: false,
  bot: false,
  group: false,
})

const me = account('1', 'alice', 'Alice')
const sage = account('3', 'sage', 'Sage')

const privateStatus = {
  id: '7',
  created_at: '2026-01-01T00:00:00.000Z',
  in_reply_to_id: null,
  in_reply_to_account_id: null,
  sensitive: false,
  spoiler_text: '',
  visibility: 'private',
  language: 'en',
  uri: 'https://example.invalid/users/sage/statuses/7',
  url: 'https://example.invalid/@sage/7',
  replies_count: 0,
  reblogs_count: 0,
  favourites_count: 0,
  edited_at: null,
  content: '<p>a followers-only thought</p>',
  reblog: null,
  account: sage,
  media_attachments: [],
  mentions: [],
  tags: [],
  emojis: [],
  card: null,
  poll: null,
  favourited: false,
  reblogged: false,
  muted: false,
  bookmarked: false,
}

async function signedIn(page: import('@playwright/test').Page, timeline: unknown[] = []) {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/verify_credentials**', (r) =>
    r.fulfill({ json: { ...me, source: { privacy: 'public' } } }),
  )
  await page.route('**/api/v1/notifications/unread_count**', (r) =>
    r.fulfill({ json: { count: 0 } }),
  )
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: timeline }))
}

const audience = (page: import('@playwright/test').Page) =>
  page.getByRole('button', { name: 'Who can see this' })

// "Quiet public" is not a visibility any more: it is a public post with
// discoverability switched off. The test is what gets *sent*, because the
// label says "Public" either way and cannot tell you which one you picked.
test('turning off discoverability posts as unlisted', async ({ page }) => {
  await signedIn(page)
  let posted: Record<string, unknown> | null = null
  await page.route('**/api/v1/statuses', async (route) => {
    posted = route.request().postDataJSON()
    await route.fulfill({ json: { ...privateStatus, id: '9' } })
  })

  await page.goto('/')
  await page.getByRole('button', { name: 'New post', exact: true }).click()
  await audience(page).click()
  await page.getByRole('menuitem', { name: /Discoverable/ }).click()
  await page.keyboard.press('Escape')

  await page.getByRole('textbox').fill('quietly')
  await page.getByRole('button', { name: 'Publish' }).click()

  await expect.poll(() => posted).toMatchObject({ visibility: 'unlisted' })
})

// Followers-only posts cannot be quoted and are not discoverable, so both
// switches are fixed off rather than merely unchecked.
test('choosing Followers disables discoverability and quoting', async ({ page }) => {
  await signedIn(page)
  await page.goto('/')
  await page.getByRole('button', { name: 'New post', exact: true }).click()
  await audience(page).click()
  await page
    .getByLabel('Visibility')
    .getByRole('menuitemradio', { name: 'Followers' })
    .click()
  await audience(page).click()

  await expect(page.getByRole('menuitem', { name: /Discoverable/ })).toHaveAttribute(
    'aria-disabled',
    'true',
  )
  await expect(
    page.getByRole('menuitem', { name: /Allow others to quote/ }),
  ).toHaveAttribute('aria-disabled', 'true')
  await expect(page.getByLabel('Who can quote')).toHaveCount(0)
})

// Post → message is immediate; message → post asks first, because that is the
// direction that can publish something written in private.
test('switching a message back to a post asks first', async ({ page }) => {
  await signedIn(page)
  await page.goto('/')
  await page.getByRole('button', { name: 'New post', exact: true }).click()

  await audience(page).click()
  await page.getByRole('menuitem', { name: 'Compose a message instead' }).click()
  await expect(page.getByRole('heading', { name: 'New message' })).toBeVisible()
  await expect(page.getByText(/not end-to-end encrypted/)).toBeVisible()

  await audience(page).click()
  await page.getByRole('menuitem', { name: 'Compose a post instead' }).click()
  await expect(page.getByText('Convert to post?')).toBeVisible()

  // Backing out leaves it a message — the confirmation is a real gate, not a
  // notice shown on the way through. (The heading itself is inert while the
  // dialog is open, which is why it is checked after closing it.)
  await page.getByRole('button', { name: 'Back' }).click()
  await expect(page.getByRole('heading', { name: 'New message' })).toBeVisible()

  await audience(page).click()
  await page.getByRole('menuitem', { name: 'Compose a post instead' }).click()
  await page.getByRole('button', { name: 'Continue' }).click()
  await expect(page.getByRole('heading', { name: 'New post' })).toBeVisible()
})

// The label names who is about to receive this, and a reply leads with the
// account replied to — that is the consequence it exists to surface.
test('a reply to a followers-only post inherits its visibility and warns', async ({
  page,
}) => {
  await signedIn(page, [privateStatus])
  await page.goto('/')

  await page.getByRole('button', { name: 'Reply' }).first().click()
  await expect(page.getByText(/You're replying to a followers-only post/)).toBeVisible()
  // Upstream's `privacyPreference`: the more private of parent and default.
  await expect(audience(page)).toContainText('Sage, Your followers')
})

// Minimising sets the composer aside without discarding it — that is the only
// reason to have the state at all, so the draft is what the test checks.
test('a minimised composer keeps its draft', async ({ page }) => {
  await signedIn(page)
  await page.goto('/')
  await page.getByRole('button', { name: 'New post', exact: true }).click()
  await page.getByRole('textbox').fill('a draft worth keeping')

  await page.getByRole('button', { name: 'Minimise composer' }).click()
  await expect(page.getByRole('textbox')).toBeHidden()

  await page.getByRole('button', { name: 'Expand composer' }).click()
  await expect(page.getByRole('textbox')).toHaveValue('a draft worth keeping')
})

test('the corner button opens a post directly', async ({ page }) => {
  await signedIn(page)
  await page.goto('/')
  await page.getByRole('button', { name: 'Compose' }).click()

  await expect(page.getByRole('heading', { name: 'New post' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Publish' })).toBeVisible()
})

test('the composer is inset from the bottom of a phone screen', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await signedIn(page)
  await page.goto('/')
  await page.getByRole('button', { name: 'New post', exact: true }).click()
  await page.waitForFunction(() => {
    const panel = document.querySelector('h2')?.closest('.fixed')
    return !!panel && getComputedStyle(panel).transform === 'none'
  })

  const bottomGap = await page.getByRole('heading', { name: 'New post' }).evaluate((h) => {
    const panel = h.closest('.fixed')!
    return window.innerHeight - panel.getBoundingClientRect().bottom
  })
  expect(bottomGap).toBe(12)
})

// The placeholder changes with the mode, because a message needs to say the
// thing a Mastodon DM cannot show: there is no To: field, so the recipients
// are typed into the text like any other mention.
test('the placeholder tells a message writer where the recipients go', async ({
  page,
}) => {
  await signedIn(page)
  await page.goto('/')

  await page.getByRole('button', { name: 'New post', exact: true }).click()
  await expect(page.getByRole('textbox')).toHaveAttribute(
    'placeholder',
    'What would you like to say?',
  )

  await audience(page).click()
  await page.getByRole('menuitem', { name: 'Compose a message instead' }).click()
  await expect(page.getByRole('textbox')).toHaveAttribute(
    'placeholder',
    'Add your recipients and your message.',
  )
})

test('the writing area grows with its content and then scrolls', async ({ page }) => {
  await signedIn(page)
  await page.goto('/')
  await page.getByRole('button', { name: 'New post', exact: true }).click()

  // The panel zooms in on open, so a measurement taken during that reads a
  // scaled box. Wait for the transform to go before believing any number.
  const settled = () =>
    page.waitForFunction(() => {
      const panel = document.querySelector('h2')?.closest('.fixed')
      return !!panel && getComputedStyle(panel).transform === 'none'
    })

  const geometry = async () => {
    await settled()
    return page.evaluate(() => {
      const panel = document.querySelector('h2')!.closest('.fixed')!
      const scroller = panel.querySelector('.overflow-y-auto')!
      const content = scroller.firstElementChild!
      const textarea = panel.querySelector('textarea')!
      return {
        textarea: textarea.getBoundingClientRect().height,
        textareaScrolls: textarea.scrollHeight > textarea.clientHeight,
        slack:
          scroller.getBoundingClientRect().bottom -
          content.getBoundingClientRect().bottom,
      }
    })
  }

  const empty = await geometry()
  expect(empty.textarea).toBeLessThan(150)
  expect(empty.slack).toBeLessThanOrEqual(1)

  await page.getByRole('textbox').fill('a line\n'.repeat(8))
  const grown = await geometry()
  expect(grown.textarea).toBeGreaterThan(empty.textarea)

  await page.getByRole('textbox').fill('a line\n'.repeat(60))
  const full = await geometry()
  expect(full.textarea).toBeGreaterThan(grown.textarea)
  expect(full.textareaScrolls).toBe(true)
})
