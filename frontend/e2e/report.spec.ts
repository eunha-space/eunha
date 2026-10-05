import { expect, test } from '@playwright/test'

const remote = {
  id: '9',
  username: 'far',
  acct: 'far@remote.example',
  display_name: 'Far Away',
  note: '',
  url: 'https://remote.example/@far',
  uri: 'https://remote.example/users/far',
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
  discoverable: true,
  indexable: true,
}
const local = { ...remote, id: '3', username: 'bob', acct: 'bob' }

const relationship = {
  id: '3',
  following: false,
  showing_reblogs: false,
  notifying: false,
  followed_by: false,
  blocking: false,
  blocked_by: false,
  muting: false,
  muting_notifications: false,
  requested: false,
  domain_blocking: false,
  endorsed: false,
  note: '',
}

async function stubProfile(page: import('@playwright/test').Page, account: typeof local) {
  await page.addInitScript(() => {
    localStorage.setItem('eunha:accounts', JSON.stringify([{ token: 'test-token', account: { id: '1', acct: 'alice' } }]))
    localStorage.setItem('eunha:active-account', '1')
  })
  await page.route('**/api/v1/accounts/lookup**', (r) => r.fulfill({ json: account }))
  await page.route('**/api/v1/accounts/verify_credentials**', (r) =>
    r.fulfill({ json: { ...local, id: '1', username: 'alice', acct: 'alice' } }),
  )
  await page.route('**/api/v1/accounts/relationships**', (r) =>
    r.fulfill({ json: [{ ...relationship, id: account.id }] }),
  )
  await page.route(`**/api/v1/accounts/${account.id}/statuses**`, (r) => r.fulfill({ json: [] }))
  await page.route('**/api/v1/notifications/unread_count**', (r) => r.fulfill({ json: { count: 0 } }))
}

// Forwarding only means something for a remote account — there is no other
// server to tell about a local one — so the switch is offered on one and not
// the other, and what it sends has to match what was ticked.
test('reporting a remote account offers to forward, and sends what was chosen', async ({
  page,
}) => {
  await stubProfile(page, remote)
  let body: Record<string, unknown> | null = null
  await page.route('**/api/v1/reports', async (route) => {
    body = route.request().postDataJSON()
    await route.fulfill({ json: { id: '1' } })
  })

  await page.goto('/@far@remote.example')
  await page.getByRole('button', { name: /More actions/ }).click()
  await page.getByRole('menuitem', { name: 'Report account' }).click()

  await expect(page.getByText('Forward to remote.example')).toBeVisible()
  await page.getByRole('switch').click()
  await page.getByRole('button', { name: 'Report', exact: true }).click()

  await expect(page.getByText(/Reported @far@remote\.example/)).toBeVisible()
  expect(body).toMatchObject({ account_id: '9', forward: true, category: 'other' })
})

test('reporting a local account does not offer forwarding', async ({ page }) => {
  await stubProfile(page, local)
  let body: Record<string, unknown> | null = null
  await page.route('**/api/v1/reports', async (route) => {
    body = route.request().postDataJSON()
    await route.fulfill({ json: { id: '1' } })
  })

  await page.goto('/@bob')
  await page.getByRole('button', { name: /More actions/ }).click()
  await page.getByRole('menuitem', { name: 'Report account' }).click()

  await expect(page.getByText(/Forward to/)).toHaveCount(0)
  await page.getByRole('button', { name: 'Report', exact: true }).click()

  await expect(page.getByText(/Reported @bob/)).toBeVisible()
  // Absent rather than false: there is nowhere to forward a local report to.
  expect(body).toMatchObject({ account_id: '3' })
  expect(body).not.toHaveProperty('forward')
})

// With rules, "breaks a server rule" asks which ones and will not send until
// one is picked; the account's recent posts can be attached.
test('a rule violation names its rules and the posts picked', async ({ page }) => {
  await stubProfile(page, local)
  await page.route('**/api/v1/instance/rules', (r) =>
    r.fulfill({
      json: [
        { id: '1', text: 'No spam', hint: '' },
        { id: '2', text: 'Be kind', hint: '' },
      ],
    }),
  )
  await page.unroute(`**/api/v1/accounts/${local.id}/statuses**`)
  await page.route(`**/api/v1/accounts/${local.id}/statuses**`, (r) =>
    r.fulfill({
      json: [
        {
          id: '50',
          created_at: '2026-09-01T00:00:00.000Z',
          content: '<p>Buy my stuff</p>',
          spoiler_text: '',
          visibility: 'public',
          sensitive: false,
          account: local,
          media_attachments: [],
          mentions: [],
          tags: [],
          emojis: [],
          reblog: null,
          in_reply_to_id: null,
          in_reply_to_account_id: null,
          uri: 'https://example.com/50',
          url: null,
          replies_count: 0,
          reblogs_count: 0,
          favourites_count: 0,
        },
      ],
    }),
  )
  let body: Record<string, unknown> | null = null
  await page.route('**/api/v1/reports', async (route) => {
    body = route.request().postDataJSON()
    await route.fulfill({ json: { id: '1' } })
  })

  await page.goto('/@bob')
  await page.getByRole('button', { name: /More actions/ }).click()
  await page.getByRole('menuitem', { name: 'Report account' }).click()
  await page.getByRole('combobox', { name: 'Reason' }).click()
  await page.getByRole('option', { name: 'Breaks a server rule' }).click()

  const report = page.getByRole('button', { name: 'Report', exact: true })
  await expect(report).toBeDisabled()
  await page.getByRole('checkbox', { name: 'Be kind' }).click()
  await page.getByRole('checkbox', { name: /Buy my stuff/ }).click()
  await report.click()

  await expect(page.getByText(/Reported @bob/)).toBeVisible()
  expect(body).toMatchObject({
    account_id: '3',
    category: 'violation',
    rule_ids: ['2'],
    status_ids: ['50'],
  })
})
