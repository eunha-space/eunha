import { expect, test, type Page } from '@playwright/test'

// Mastodon `UserRole::FLAGS`.
const ADMINISTRATOR = 1 << 0
const MANAGE_REPORTS = 1 << 4

const account = (id: string, acct: string) => ({
  id,
  username: acct.split('@')[0],
  acct,
  display_name: acct,
  note: '',
  url: `https://example.com/@${acct}`,
  uri: `https://example.com/users/${acct}`,
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

const adminAccount = (id: string, acct: string, extra: Record<string, unknown> = {}) => ({
  id,
  username: acct.split('@')[0],
  domain: acct.includes('@') ? acct.split('@')[1] : null,
  created_at: '2026-01-01T00:00:00.000Z',
  email: `${acct.split('@')[0]}@example.com`,
  ip: '192.0.2.1',
  ips: [],
  role: { id: '-99', name: '' },
  confirmed: true,
  suspended: false,
  silenced: false,
  sensitized: false,
  disabled: false,
  approved: true,
  locale: 'en',
  invite_request: null,
  account: account(id, acct),
  ...extra,
})

const report = {
  id: '7',
  action_taken: false,
  action_taken_at: null,
  category: 'spam',
  comment: 'Selling things in replies',
  forwarded: false,
  created_at: '2026-09-30T00:00:00.000Z',
  updated_at: '2026-09-30T00:00:00.000Z',
  account: adminAccount('2', 'carol'),
  target_account: adminAccount('3', 'spammer'),
  assigned_account: null,
  action_taken_by_account: null,
  statuses: [],
  rules: [],
}

async function signIn(page: Page, permissions: number) {
  await page.addInitScript(() => localStorage.setItem('eunha:token', 'test-token'))
  await page.route('**/api/v1/accounts/verify_credentials**', (r) =>
    r.fulfill({
      json: {
        ...account('1', 'alice'),
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

test('the rail offers moderation only to a role that has it', async ({ page }) => {
  await signIn(page, 1 << 16) // invite_users alone
  await page.goto('/')
  const rail = page.locator('aside')
  await expect(rail.getByRole('link', { name: 'Home' })).toBeVisible()
  await expect(rail.getByRole('link', { name: 'Moderation' })).toHaveCount(0)
})

test('a moderator lands on the report queue and works a report', async ({ page }) => {
  await signIn(page, MANAGE_REPORTS)
  await page.route(/\/api\/v1\/admin\/reports(\?.*)?$/, (r) => r.fulfill({ json: [report] }))
  await page.route('**/api/v1/admin/reports/7', (r) => r.fulfill({ json: report }))
  await page.route('**/api/v1/admin/accounts/3', (r) =>
    r.fulfill({ json: adminAccount('3', 'spammer') }),
  )
  await page.route('**/api/v1/instance/rules', (r) => r.fulfill({ json: [] }))
  let assigned = false
  await page.route('**/api/v1/admin/reports/7/assign_to_self', (r) => {
    assigned = true
    return r.fulfill({
      json: { ...report, assigned_account: adminAccount('1', 'alice') },
    })
  })
  let action: Record<string, unknown> | null = null
  await page.route('**/api/v1/admin/accounts/3/action', async (r) => {
    action = r.request().postDataJSON()
    await r.fulfill({ json: {} })
  })

  await page.goto('/')
  await page.locator('aside').getByRole('link', { name: 'Moderation' }).click()
  // No dashboard permission, so `/admin` sends this role to the reports.
  await expect(page).toHaveURL(/\/admin\/reports$/)
  await expect(page.getByText('Selling things in replies')).toBeVisible()

  await page.getByRole('link', { name: /#7/ }).click()
  await expect(page).toHaveURL(/\/admin\/reports\/7$/)
  await page.getByRole('button', { name: 'Assign to me' }).click()
  await expect(page.getByRole('button', { name: 'Unassign' })).toBeVisible()
  expect(assigned).toBe(true)

  await page.getByRole('button', { name: 'Limit', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog.getByText('Moderate @spammer')).toBeVisible()
  await dialog.getByRole('button', { name: 'Limit', exact: true }).click()
  await expect.poll(() => action).toMatchObject({ type: 'silence', report_id: '7' })
})

test('a remote account is offered no warning or freeze', async ({ page }) => {
  await signIn(page, ADMINISTRATOR)
  await page.route('**/api/v1/admin/accounts/9', (r) =>
    r.fulfill({ json: adminAccount('9', 'far@remote.example', { email: null, ip: null }) }),
  )
  await page.goto('/admin/accounts/9')
  await page.getByRole('button', { name: 'Moderate…' }).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog.getByText('Suspend', { exact: true })).toBeVisible()
  await expect(dialog.getByText('Warning', { exact: true })).toHaveCount(0)
  await expect(dialog.getByText('Freeze', { exact: true })).toHaveCount(0)
})

test('the account filters reach the v2 admin account list', async ({ page }) => {
  await signIn(page, ADMINISTRATOR)
  const queries: string[] = []
  await page.route('**/api/v2/admin/accounts**', (r) => {
    queries.push(new URL(r.request().url()).search)
    return r.fulfill({
      json: [adminAccount('4', 'newbie', { approved: false, invite_request: 'Hi!' })],
    })
  })
  await page.goto('/admin/accounts?status=pending')
  await expect(page.getByText('“Hi!”')).toBeVisible()
  expect(queries.some((q) => q.includes('status=pending'))).toBe(true)
})

test('a domain block that already exists is offered for editing', async ({ page }) => {
  await signIn(page, ADMINISTRATOR)
  const existing = {
    id: '5',
    domain: 'bad.example',
    digest: '',
    created_at: '2026-09-01T00:00:00.000Z',
    severity: 'suspend',
    reject_media: false,
    reject_reports: false,
    private_comment: null,
    public_comment: null,
    obfuscate: false,
  }
  await page.route('**/api/v1/admin/domain_blocks', (r) =>
    r.request().method() === 'POST'
      ? r.fulfill({
          status: 422,
          json: { error: 'A domain block already exists', existing_domain_block: existing },
        })
      : r.fulfill({ json: [] }),
  )
  let patched: Record<string, unknown> | null = null
  await page.route('**/api/v1/admin/domain_blocks/5', async (r) => {
    patched = r.request().postDataJSON()
    await r.fulfill({ json: { ...existing, ...patched } })
  })

  await page.goto('/admin/domain_blocks')
  await page.getByRole('button', { name: 'Add domain block' }).click()
  await page.getByRole('textbox', { name: 'Domain' }).fill('bad.example')
  await page.getByRole('button', { name: 'Block domain' }).click()
  await page.getByRole('button', { name: 'Edit the existing block instead' }).click()
  await page.getByRole('button', { name: 'Save' }).click()
  await expect.poll(() => patched).toMatchObject({ severity: 'silence' })
  await expect(page.getByText('bad.example', { exact: true })).toBeVisible()
})

test('a report notification links to the report', async ({ page }) => {
  await signIn(page, MANAGE_REPORTS)
  await page.route('**/api/v1/notifications?**', (r) =>
    r.fulfill({
      json: [
        {
          id: '100',
          type: 'admin.report',
          created_at: '2026-09-30T00:00:00.000Z',
          account: account('2', 'carol'),
          report: {
            id: '7',
            action_taken: false,
            category: 'spam',
            comment: 'Selling things',
            forwarded: false,
            created_at: '2026-09-30T00:00:00.000Z',
            status_ids: ['11'],
            rule_ids: [],
            target_account: account('3', 'spammer'),
          },
        },
      ],
    }),
  )
  await page.route('**/api/v1/markers**', (r) => r.fulfill({ json: {} }))
  await page.goto('/notifications')
  const link = page.getByRole('link', { name: /Report on @spammer/ })
  await expect(link).toHaveAttribute('href', '/admin/reports/7')
})

test('the audit log words each entry and links its target', async ({ page }) => {
  await signIn(page, ADMINISTRATOR)
  await page.route('**/api/v1/admin/action_logs/filters', (r) =>
    r.fulfill({
      json: {
        accounts: [{ key: '1', label: 'alice' }],
        action_types: [{ key: 'silence_account', label: 'Limit Account' }],
      },
    }),
  )
  await page.route(/\/api\/v1\/admin\/action_logs(\?.*)?$/, (r) =>
    r.fulfill({
      json: [
        {
          id: '40',
          action: 'silence',
          action_type: 'silence_account',
          target_type: 'Account',
          target_id: '3',
          created_at: '2026-09-30T00:00:00.000Z',
          account: account('1', 'alice'),
          template: "%{name} limited %{target}'s account",
          target: { text: 'spammer', href: '/admin/accounts/3' },
          changes: null,
          text: "alice limited spammer's account",
        },
      ],
    }),
  )
  await page.goto('/admin/action_logs')
  await expect(page.getByText("limited", { exact: false })).toBeVisible()
  await expect(page.getByRole('link', { name: 'spammer' })).toHaveAttribute(
    'href',
    '/admin/accounts/3',
  )
})

test('the moderation form offers warning presets', async ({ page }) => {
  await signIn(page, ADMINISTRATOR)
  await page.route('**/api/v1/admin/accounts/3', (r) =>
    r.fulfill({ json: adminAccount('3', 'spammer') }),
  )
  await page.route('**/api/v1/admin/account_moderation_notes**', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/admin/roles', (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/admin/warning_presets', (r) =>
    r.fulfill({
      json: [{ id: '8', title: 'Spam', text: 'Stop spamming.', created_at: '2026-09-01T00:00:00Z' }],
    }),
  )
  let action: Record<string, unknown> | null = null
  await page.route('**/api/v1/admin/accounts/3/action', async (r) => {
    action = r.request().postDataJSON()
    await r.fulfill({ json: {} })
  })
  await page.goto('/admin/accounts/3')
  await page.getByRole('button', { name: 'Moderate…' }).click()
  const dialog = page.getByRole('dialog')
  await dialog.getByRole('combobox', { name: 'Warning preset' }).click()
  await page.getByRole('option', { name: 'Spam' }).click()
  await expect(dialog.getByText('Stop spamming.')).toBeVisible()
  await dialog.getByRole('button', { name: 'Warning', exact: true }).click()
  await expect.poll(() => action).toMatchObject({ type: 'none', warning_preset_id: '8' })
})

test('a user appeals a strike against them', async ({ page }) => {
  await signIn(page, 1 << 16)
  const strike = {
    id: '21',
    action: 'silence',
    text: 'Too loud.',
    status_ids: null,
    created_at: '2026-09-30T00:00:00.000Z',
    target_account: account('1', 'alice'),
    appeal: null,
    overruled_at: null,
    appeal_eligible: true,
    appeal_deadline: '2026-10-20T00:00:00.000Z',
    can_appeal: true,
    statuses: [],
  }
  let appealed: Record<string, unknown> | null = null
  await page.route('**/api/v1/disputes/strikes/21', (r) => r.fulfill({ json: strike }))
  await page.route('**/api/v1/disputes/strikes/21/appeal', async (r) => {
    appealed = r.request().postDataJSON()
    await r.fulfill({
      json: {
        ...strike,
        can_appeal: false,
        appeal: {
          id: '5',
          text: 'I was quiet',
          state: 'pending',
          created_at: '2026-10-01T00:00:00.000Z',
          approved_at: null,
          rejected_at: null,
        },
      },
    })
  })
  await page.goto('/disputes/strikes/21')
  await expect(page.getByRole('heading', { name: /Limitation of account/ })).toBeVisible()
  await page.getByRole('textbox').fill('I was quiet')
  await page.getByRole('button', { name: 'Submit appeal' }).click()
  await expect.poll(() => appealed).toMatchObject({ text: 'I was quiet' })
  await expect(page.getByRole('heading', { name: 'Appeal', exact: true })).toBeVisible()
})
