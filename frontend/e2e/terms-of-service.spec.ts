import { expect, test, type Page } from '@playwright/test'

// Mastodon `UserRole::FLAGS`.
const MANAGE_SETTINGS = 1 << 6
const MANAGE_REPORTS = 1 << 4

async function signIn(page: Page, permissions: number) {
  await page.addInitScript(() => localStorage.setItem('eunha:token', 'test-token'))
  await page.route('**/api/v1/accounts/verify_credentials**', (r) =>
    r.fulfill({
      json: {
        id: '1',
        username: 'alice',
        acct: 'alice',
        display_name: 'alice',
        note: '',
        url: 'https://example.com/@alice',
        avatar: '',
        avatar_static: '',
        header: '',
        header_static: '',
        followers_count: 0,
        following_count: 0,
        statuses_count: 0,
        created_at: '2026-01-01T00:00:00.000Z',
        emojis: [],
        fields: [],
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

const version = {
  text: 'Be kind.',
  changelog: 'Kindness.',
  effective: false,
  text_html: '<p>Be kind.</p>\n',
  changelog_html: '<p>Kindness.</p>\n',
  notification_sent_at: null,
}

test('terms of service are offered only to a role with manage_settings', async ({ page }) => {
  await signIn(page, MANAGE_REPORTS)
  await page.route(/\/api\/v1\/admin\/reports(\?.*)?$/, (r) => r.fulfill({ json: [] }))
  await page.goto('/admin/reports')
  await expect(
    page.getByRole('navigation', { name: 'Moderation sections' }).getByRole('link', {
      name: 'Terms of service',
    }),
  ).toHaveCount(0)
})

test('an administrator drafts, publishes and notifies', async ({ page }) => {
  await signIn(page, MANAGE_SETTINGS)
  let published = false
  await page.route('**/api/v1/admin/terms_of_service', (r) =>
    published
      ? r.fulfill({
          json: {
            ...version,
            id: '5',
            effective_date: '2026-10-12',
            published_at: '2026-10-02T00:00:00.000Z',
          },
        })
      : r.fulfill({ status: 404, json: { error: 'Record not found' } }),
  )
  await page.route('**/api/v1/admin/terms_of_service/draft', async (r) => {
    if (r.request().method() === 'GET') {
      return r.fulfill({
        json: {
          ...version,
          id: null,
          text: '',
          changelog: '',
          effective_date: '2026-10-12',
          published_at: null,
        },
      })
    }
    const body = r.request().postDataJSON() as Record<string, string>
    if (body.action_type === 'publish' && !body.changelog) {
      return r.fulfill({
        status: 422,
        json: { error: "Validation failed: Changelog can't be blank" },
      })
    }
    published = body.action_type === 'publish'
    return r.fulfill({
      json: {
        ...version,
        id: '5',
        text: body.text,
        changelog: body.changelog,
        effective_date: body.effective_date,
        published_at: published ? '2026-10-02T00:00:00.000Z' : null,
      },
    })
  })
  let distributed = false
  await page.route('**/api/v1/admin/terms_of_service/5/preview', (r) =>
    r.fulfill({
      json: {
        user_count: 3,
        terms_of_service: {
          ...version,
          id: '5',
          effective_date: '2026-10-12',
          published_at: '2026-10-02T00:00:00.000Z',
        },
      },
    }),
  )
  await page.route('**/api/v1/admin/terms_of_service/5/distribution', (r) => {
    distributed = true
    return r.fulfill({ json: { ...version, id: '5' } })
  })

  await page.goto('/admin/terms_of_service')
  await expect(page.getByText("You don't currently have any terms of service")).toBeVisible()
  await page.getByRole('link', { name: 'Use your own' }).click()
  await expect(page).toHaveURL(/\/admin\/terms_of_service\/draft$/)

  await page.getByRole('textbox', { name: 'Terms of Service' }).fill('Be kind.')
  await page.locator('form').getByRole('button', { name: 'Publish' }).click()
  await page.getByRole('alertdialog').getByRole('button', { name: 'Publish' }).click()
  await expect(page.getByText("Validation failed: Changelog can't be blank")).toBeVisible()

  await page.getByRole('textbox', { name: "What's changed?" }).fill('Kindness.')
  await page.locator('form').getByRole('button', { name: 'Publish' }).click()
  await page.getByRole('alertdialog').getByRole('button', { name: 'Publish' }).click()
  await expect(page).toHaveURL(/\/admin\/terms_of_service$/)
  await expect(page.getByText('Be kind.')).toBeVisible()

  await page.getByRole('link', { name: 'Notify users' }).click()
  await expect(page.getByText('3 users')).toBeVisible()
  await page.getByRole('button', { name: 'Send 3 emails' }).click()
  await page.getByRole('alertdialog').getByRole('button', { name: 'Send 3 emails' }).click()
  await expect.poll(() => distributed).toBe(true)
})

test('a flagged user is shown the new terms until they open them', async ({ page }) => {
  await signIn(page, 0)
  let dismissed = false
  await page.route('**/api/eunha/v1/terms_of_service/interstitial', (r) => {
    if (r.request().method() === 'DELETE') {
      dismissed = true
      return r.fulfill({ json: {} })
    }
    return r.fulfill({
      json: {
        terms_of_service: dismissed
          ? null
          : {
              effective_date: '2026-10-12',
              effective: false,
              content: '<p>New terms.</p>\n',
              succeeded_by: null,
            },
      },
    })
  })
  await page.route('**/api/v1/instance/terms_of_service', (r) =>
    r.fulfill({
      json: {
        effective_date: '2026-10-12',
        effective: false,
        content: '<p>New terms.</p>\n',
        succeeded_by: '2027-01-01',
      },
    }),
  )

  await page.goto('/')
  const dialog = page.getByRole('dialog')
  await expect(dialog.getByText(/terms of service of .* are changing/)).toBeVisible()
  await dialog.getByRole('link', { name: 'Review terms of service' }).click()
  await expect(page).toHaveURL(/\/terms-of-service$/)
  await expect(page.getByText('New terms.')).toBeVisible()
  await expect(page.getByText(/Effective as of/)).toBeVisible()
  await expect(page.getByRole('link', { name: /Upcoming changes on/ })).toHaveAttribute(
    'href',
    '/terms-of-service/2027-01-01',
  )
  await expect.poll(() => dismissed).toBe(true)
  await expect(page.getByRole('dialog')).toHaveCount(0)
})
