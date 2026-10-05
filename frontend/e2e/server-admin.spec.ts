import { expect, test, type Page } from '@playwright/test'

// Mastodon `UserRole::FLAGS`.
const MANAGE_SETTINGS = 1 << 6
const MANAGE_RULES = 1 << 12

const account = {
  id: '1',
  username: 'alice',
  acct: 'alice',
  display_name: 'alice',
  note: '',
  url: 'https://example.com/@alice',
  uri: 'https://example.com/users/alice',
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
}

async function signIn(page: Page, permissions: number) {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/verify_credentials**', (r) =>
    r.fulfill({
      json: {
        ...account,
        source: { privacy: 'public' },
        role: { id: '3', name: 'Admin', permissions: String(permissions) },
      },
    }),
  )
  await page.route('**/api/v1/notifications/unread_count**', (r) =>
    r.fulfill({ json: { count: 0 } }),
  )
  await page.route('**/api/v1/timelines/**', (r) => r.fulfill({ json: [] }))
}

const settings = {
  site_title: 'Galaxy',
  site_contact_username: '',
  site_contact_email: '',
  site_short_description: '',
  site_extended_description: '',
  site_terms: '',
  registrations_mode: 'open',
  closed_registrations_message: '',
  bootstrap_timeline_accounts: '',
  theme: 'default',
  require_invite_text: false,
  captcha_enabled: false,
  min_age: null,
  thumbnail: null,
  mascot: null,
  app_icon: null,
  favicon: null,
  overridden: [],
}

test('a settings page saves its own keys and no others', async ({ page }) => {
  await signIn(page, MANAGE_SETTINGS)
  let saved: string | null = null
  await page.route('**/api/v1/admin/settings', async (r) => {
    if (r.request().method() === 'PATCH') {
      saved = r.request().postData()
      await r.fulfill({ json: { ...settings, require_invite_text: true } })
    } else {
      await r.fulfill({ json: settings })
    }
  })

  await page.goto('/admin/settings/registrations')
  await expect(page.getByRole('heading', { name: /Registrations/ })).toBeVisible()
  await page.getByText('Require a reason to join').click()
  await page.getByRole('button', { name: 'Save changes' }).click()
  await expect.poll(() => saved).toContain('name="require_invite_text"')
  expect(saved).toContain('name="registrations_mode"')
  expect(saved).not.toContain('name="site_title"')
})

test('a rule is added and moved', async ({ page }) => {
  await signIn(page, MANAGE_RULES)
  const rule = (id: string, text: string) => ({
    id,
    text,
    hint: '',
    priority: 0,
    translations: [],
    created_at: '2026-09-30T00:00:00.000Z',
    updated_at: '2026-09-30T00:00:00.000Z',
  })
  let rules = [rule('1', 'Be kind')]
  let created: Record<string, unknown> | null = null
  await page.route('**/api/v1/admin/rules', async (r) => {
    if (r.request().method() === 'POST') {
      created = r.request().postDataJSON()
      rules = [...rules, rule('2', 'No spam')]
      await r.fulfill({ json: rules[1] })
    } else {
      await r.fulfill({ json: rules })
    }
  })
  await page.route('**/api/v1/admin/rules/2/move_up', async (r) => {
    rules = [rules[1], rules[0]]
    await r.fulfill({ json: rules })
  })

  await page.goto('/admin/rules')
  await expect(page.getByText('Be kind')).toBeVisible()
  await page.getByRole('textbox', { name: 'Rule', exact: true }).fill('No spam')
  await page.getByRole('button', { name: 'Add rule' }).click()
  await expect.poll(() => created).toMatchObject({ text: 'No spam' })
  await expect(page.getByText('No spam')).toBeVisible()
  await page.getByRole('button', { name: 'Move up' }).nth(1).click()
  await expect(page.locator('ol li').first()).toContainText('No spam')
})
