import { expect, test } from '@playwright/test'

const base = {
  domain: 'example.invalid',
  title: 'Example',
  version: '0.2.0 (compatible; Mastodon 4.7.3)',
  source_url: 'https://github.com/limeburst/eunha',
  description: 'A place.',
  usage: { users: { active_month: 4 } },
  languages: ['ko', 'en'],
  configuration: {},
  registrations: { enabled: false, approval_required: false },
  contact: { email: 'admin@example.invalid', account: null },
  rules: [],
  api_versions: {},
}

// Every one of these is optional on the wire. An instance that publishes
// nothing must not get a page of empty headings — so the test that matters is
// which sections are absent, not which are present. The privacy policy is the
// exception: Mastodon always has one, its own if nothing else.
test('about renders only the sections the instance actually publishes', async ({
  page,
}) => {
  await page.route('**/api/v2/instance', (r) => r.fulfill({ json: base }))

  await page.goto('/about')
  await expect(page.getByRole('heading', { name: 'Example' })).toBeVisible()
  await expect(page.getByText('Closed — new accounts are by invitation only.')).toBeVisible()
  await expect(page.getByText('4 people have posted in the last month.')).toBeVisible()

  await expect(page.getByRole('link', { name: 'Privacy policy' })).toBeVisible()
  // No `urls.terms_of_service`: nothing has been published.
  await expect(page.getByRole('link', { name: 'Terms of service' })).toHaveCount(0)
  await expect(page.getByRole('heading', { name: 'Rules' })).toHaveCount(0)
  // No contact account was returned, so there is nobody to name.
  await expect(page.getByRole('heading', { name: 'Run by' })).toHaveCount(0)
})

test('about links the policy documents when an instance has them', async ({ page }) => {
  await page.route('**/api/v2/instance', (r) =>
    r.fulfill({
      json: {
        ...base,
        configuration: {
          urls: { terms_of_service: 'https://example.invalid/terms-of-service' },
        },
        registrations: { enabled: true, approval_required: true },
        rules: [{ id: '1', text: 'Be decent to each other.' }],
      },
    }),
  )
  await page.route('**/api/v1/instance/terms_of_service', (r) =>
    r.fulfill({
      json: {
        effective_date: '2025-01-01',
        effective: true,
        content: '<h2>Play nice</h2>\n<p>Or else.</p>\n',
        succeeded_by: null,
      },
    }),
  )

  await page.goto('/about')
  await expect(
    page.getByText('Open, and each new account is reviewed before it can sign in.'),
  ).toBeVisible()
  await expect(page.getByText('Be decent to each other.')).toBeVisible()

  await page.getByRole('link', { name: 'Terms of service' }).click()
  await expect(page).toHaveURL(/\/terms-of-service$/)
  // The server's HTML, rendered as HTML.
  await expect(page.getByRole('heading', { name: 'Play nice' })).toBeVisible()
  await expect(page.getByText('Last updated')).toBeVisible()
})
