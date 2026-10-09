import { expect, test, type Page, type Locator } from '@playwright/test'

const account = {
  id: '1', username: 'alice', acct: 'alice', display_name: 'Alice', note: '',
  url: 'https://example.invalid/@alice', uri: 'https://example.invalid/users/alice',
  avatar: '', avatar_static: '', header: '', header_static: '', emojis: [], fields: [],
  followers_count: 0, following_count: 0, statuses_count: 1,
  created_at: '2026-01-01T00:00:00Z', locked: false, bot: false, group: false,
}
const image = (id: string, description: string | null) => ({
  id, type: 'image', url: `/media/${id}.svg`, preview_url: `/media/preview-${id}.svg`,
  remote_url: null, description, blurhash: null, meta: {},
})
const attachments = [image('1', 'A blue portrait'), image('2', 'A green landscape')]
const post = {
  id: '10', account, content: '<p>Pictures from today</p>', created_at: '2026-01-01T00:00:00Z',
  sensitive: false, spoiler_text: '', visibility: 'public', language: 'en',
  uri: 'https://example.invalid/users/alice/statuses/10', url: 'https://example.invalid/@alice/10',
  replies_count: 0, reblogs_count: 0, favourites_count: 0, edited_at: null,
  media_attachments: attachments, mentions: [], tags: [], emojis: [], reblog: null, card: null, poll: null,
}

async function setup(page: Page, changes: Partial<typeof post> = {}) {
  await page.route('**/api/v1/statuses/10', r => r.fulfill({ json: { ...post, ...changes } }))
  await page.route('**/api/v1/statuses/10/context', r => r.fulfill({ json: { ancestors: [], descendants: [] } }))
  await page.route('**/api/v2/instance', r => r.fulfill({ json: {
    title: 'Example', domain: 'example.invalid', version: '4.7.2', configuration: {},
    registrations: { enabled: false, approval_required: false },
  } }))
  await page.route('**/media/*.svg', r => {
    const portrait = r.request().url().includes('1.svg')
    return r.fulfill({ contentType: 'image/svg+xml', body: `<svg xmlns="http://www.w3.org/2000/svg" width="${portrait ? 900 : 1400}" height="${portrait ? 1400 : 900}"><rect width="100%" height="100%" fill="${portrait ? '#287da8' : '#4b915e'}" /></svg>` })
  })
  await page.goto('/@alice/10')
}

async function swipe(picture: Locator, from: number, to: number) {
  await picture.evaluate((element, { from, to }) => {
    const touch = (x: number) => new Touch({ identifier: 1, target: element, clientX: x, clientY: 300 })
    element.dispatchEvent(new TouchEvent('touchstart', { bubbles: true, touches: [touch(from)] }))
    element.dispatchEvent(new TouchEvent('touchend', { bubbles: true, changedTouches: [touch(to)] }))
  }, { from, to })
}

test('images open in-app with gallery navigation and return keyboard focus', async ({ page, context }) => {
  await setup(page)
  const trigger = page.getByRole('button', { name: 'View image: A blue portrait' })
  await trigger.click()
  const viewer = page.getByRole('dialog')
  await expect(viewer).toBeVisible()
  await expect(viewer.getByRole('img', { name: 'A blue portrait' })).toBeVisible()
  await expect(viewer.getByText('Image 1 of 2')).toBeVisible()
  await expect(viewer.getByRole('button', { name: 'Previous image' })).toBeDisabled()
  await viewer.getByRole('button', { name: 'Next image' }).click()
  await expect(viewer.getByRole('img', { name: 'A green landscape' })).toBeVisible()
  await expect(viewer.getByRole('button', { name: 'Next image' })).toBeDisabled()
  await page.keyboard.press('ArrowLeft')
  await expect(viewer.getByRole('img', { name: 'A blue portrait' })).toBeVisible()
  expect(context.pages()).toHaveLength(1)
  await page.keyboard.press('Escape')
  await expect(viewer).toHaveCount(0)
  await expect(trigger).toBeFocused()
  await page.getByRole('button', { name: 'View image: A green landscape' }).click()
  await expect(page.getByRole('dialog').getByText('Image 2 of 2')).toBeVisible()
  await page.getByRole('button', { name: 'Close image viewer' }).click()
  await expect(page.getByRole('dialog')).toHaveCount(0)
})

test('sensitive images must be revealed before the viewer opens', async ({ page }) => {
  await setup(page, { sensitive: true })
  await expect(page.getByRole('button', { name: 'View image: A blue portrait' })).toHaveCount(0)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await page.getByRole('button', { name: /Sensitive content/ }).click()
  await page.getByRole('button', { name: 'View image: A blue portrait' }).click()
  await expect(page.getByRole('dialog')).toBeVisible()
})

test('single image handles a missing description and failed original', async ({ page }) => {
  await setup(page, { media_attachments: [image('1', null)] })
  await page.route('**/media/1.svg', r => r.fulfill({ status: 404 }))
  await page.getByRole('button', { name: 'View image attachment' }).click()
  const viewer = page.getByRole('dialog')
  await expect(viewer.getByRole('alert')).toContainText('Could not load this image')
  await expect(viewer.getByText('No image description provided.')).toBeVisible()
  await expect(viewer.getByRole('button', { name: 'Next image' })).toHaveCount(0)
  await expect(viewer.getByRole('link', { name: 'Open original image in a new tab' })).toHaveAttribute('href', '/media/1.svg')
})

test('mobile viewer fits tall images, swipes between images and supports original size', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await setup(page)
  await page.getByRole('button', { name: 'View image: A blue portrait' }).click()
  const viewer = page.getByRole('dialog')
  const picture = viewer.getByRole('img', { name: 'A blue portrait' })
  await expect(picture).toBeVisible()
  const bounds = await picture.boundingBox()
  expect(bounds!.width).toBeLessThanOrEqual(390)
  expect(bounds!.height).toBeLessThan(844)
  await swipe(picture, 300, 80)
  await expect(viewer.getByRole('img', { name: 'A green landscape' })).toBeVisible()
  await viewer.getByRole('button', { name: 'View image at original size' }).click()
  await expect(viewer.getByRole('button', { name: 'Fit image to screen' })).toBeVisible()
  const original = viewer.getByRole('img', { name: 'A green landscape' })
  await swipe(original, 80, 300)
  await expect(original).toBeVisible()
  await viewer.getByRole('button', { name: 'Fit image to screen' }).click()
  await expect(viewer.getByRole('button', { name: 'View image at original size' })).toBeVisible()
  await page.screenshot({ path: '/tmp/eunha-image-viewer-mobile.png' })
})


test('mixed media keeps inline players and the gallery includes only images', async ({ page }) => {
  await setup(page, { media_attachments: [attachments[0],
    { ...image('video', 'A video'), type: 'video', url: '/media/video.mp4' },
    attachments[1], { ...image('audio', 'Audio'), type: 'audio', url: '/media/audio.mp3' },
  ] })
  await expect(page.locator('video[controls]')).toHaveCount(1)
  await expect(page.locator('audio[controls]')).toHaveCount(1)
  await page.getByRole('button', { name: 'View image: A green landscape' }).click()
  await expect(page.getByRole('dialog').getByText('Image 2 of 2')).toBeVisible()
})

test('moderation image previews use the same in-app viewer', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/verify_credentials**', r => r.fulfill({ json: {
    ...account, source: { privacy: 'public' }, role: { id: '1', name: 'Staff', permissions: String(1 << 10) },
  } }))
  await page.route('**/api/v1/invites', r => r.fulfill({ json: [] }))
  await page.route('**/api/v1/notifications/unread_count**', r => r.fulfill({ json: { count: 0 } }))
  await page.route('**/api/eunha/v1/terms_of_service/interstitial', r => r.fulfill({ status: 204 }))
  await page.route('**/api/v1/admin/accounts/1/statuses/10', r => r.fulfill({ json: { ...post, edits: [] } }))
  await setup(page)
  await page.goto('/admin/accounts/1/statuses/10')
  await page.getByRole('button', { name: 'View image: A blue portrait' }).click()
  await expect(page.getByRole('dialog').getByRole('img', { name: 'A blue portrait' })).toBeVisible()
  await page.getByRole('button', { name: 'Next image' }).click()
  await expect(page.getByRole('dialog').getByRole('img', { name: 'A green landscape' })).toBeVisible()
})
