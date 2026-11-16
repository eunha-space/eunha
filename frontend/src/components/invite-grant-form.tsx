import { useEffect, useState } from 'react'
import { useSearchParams } from 'react-router-dom'
import { toast } from 'sonner'
import { getGrantRecipients, grantInvites } from '../eunha-api.ts'
import { getToken } from '../auth.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { ChoiceSelect } from '@/components/admin/admin-common.tsx'
import { AccountListSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const EXPIRIES = { '604800': '7 days', '86400': '1 day', get '0'() { return t({ id: 'mastodon.invites.expires_in_prompt', defaultMessage: 'Never' }) } }

export function InviteGrantForm({ onGranted }: { onGranted?: () => void }) {
  const token = getToken()
  const [params] = useSearchParams()
  const [members, setMembers] = useState<{ id: string; acct: string }[] | null>(
    null,
  )
  const [error, setError] = useState<string | null>(null)
  const [retry, setRetry] = useState(0)
  const [recipient, setRecipient] = useState(params.get('grant_to') ?? '')
  const [query, setQuery] = useState('')
  const [count, setCount] = useState('3')
  const [expiry, setExpiry] = useState('604800')
  const [review, setReview] = useState(false)
  const [pending, setPending] = useState(false)
  const [result, setResult] = useState<{
    granted: number
    accounts: number
  } | null>(null)
  useEffect(() => {
    if (!token) return
    let cancelled = false
    setError(null)
    setMembers(null)
    getGrantRecipients(token)
      .then((m) => {
        if (!cancelled) setMembers(m)
      })
      .catch(() => {
        if (!cancelled)
          setError(t({ id: 'eunha.invite_grant_form.could_not_load_members_try_again', defaultMessage: 'Could not load members. Try again before granting invites.' }))
      })
    return () => {
      cancelled = true
    }
  }, [token, retry])
  const selected = members?.find((m) => m.id === recipient)
  const recipients =
    recipient === 'everyone' ? (members?.length ?? 0) : selected ? 1 : 0
  const number = Number(count)
  const ready =
    !!members &&
    recipients > 0 &&
    Number.isInteger(number) &&
    number >= 1 &&
    number <= 25
  const items = {
    '': t({ id: 'eunha.invite_grant_form.choose_a_recipient', defaultMessage: 'Choose a recipient' }),
    everyone: t({ id: 'eunha.invite_grant_form.all_local_users', defaultMessage: 'All local users ({count})' }, { count: members?.length ?? 0 }),
    ...Object.fromEntries((members ?? []).map((m) => [m.id, `@${m.acct}`])),
  }
  const visibleItems = {
    '': t({ id: 'eunha.invite_grant_form.choose_a_recipient', defaultMessage: 'Choose a recipient' }),
    everyone: items.everyone,
    ...Object.fromEntries(
      (members ?? [])
        .filter(
          (m) =>
            m.id === recipient ||
            m.acct.toLowerCase().includes(query.toLowerCase()),
        )
        .map((m) => [m.id, `@${m.acct}`]),
    ),
  }
  const submit = async () => {
    if (!token || !ready || pending) return
    setPending(true)
    setError(null)
    try {
      const r = await grantInvites(token, {
        ...(recipient === 'everyone' ? { all: true as const } : { account_id: recipient }),
        count: number,
        max_uses: 1,
        expires_in: expiry === '0' ? undefined : Number(expiry),
      })
      setResult(r)
      setReview(false)
      onGranted?.()
      toast.success(t({ id: 'eunha.invite_grant_form.granted_invites_to_local_users', defaultMessage: 'Granted {granted} invites to {accounts} local users' }, { granted: r.granted, accounts: r.accounts }))
    } catch {
      setError(t({ id: 'eunha.invite_grant_form.could_not_grant_invites_please_try', defaultMessage: 'Could not grant invites. Please try again.' }))
    } finally {
      setPending(false)
    }
  }
  return (
    <section
      className="mb-6 space-y-4 rounded-lg border p-4"
      aria-label={t({ id: 'eunha.invite_grant_form.grant_invites', defaultMessage: 'Grant invites' })}
    >
      <div>
        <h2 className="text-base font-semibold">{t({ id: 'eunha.invite_grant_form.grant_invites', defaultMessage: 'Grant invites' })}</h2>
        <p className="text-muted-foreground text-sm">
          {t({ id: 'eunha.invite_grant_form.give_members_people_to_bring_along', defaultMessage: 'Give members people to bring along. Their invitees join the tree under them.' })}
        </p>
      </div>
      {error && (
        <p role="alert" className="text-destructive text-sm">
          {error}{' '}
          {!members && (
            <Button
              size="sm"
              variant="outline"
              onClick={() => setRetry((n) => n + 1)}
            >
              {t({ id: 'bundle_column_error.retry', defaultMessage: 'Try again' })}
            </Button>
          )}
        </p>
      )}
      {result ? (
        <div className="space-y-3" role="status">
          <h3 className="font-medium">
            {t(
              {
                id: 'eunha.invite_grant_form.granted',
                defaultMessage: '{granted} invites granted to {accounts, plural, one {# local user} other {# local users}}',
              },
              { granted: result.granted, accounts: result.accounts },
            )}
          </h3>
          <p className="text-muted-foreground text-sm">
            {t({ id: 'eunha.invite_grant_form.links_are_ready_on_each_recipient', defaultMessage: 'Links are ready on each recipient’s Your invites page.' })}
          </p>
          <Button
            variant="outline"
            onClick={() => {
              setResult(null)
              setRetry((n) => n + 1)
            }}
          >
            {t({ id: 'eunha.invite_grant_form.grant_more_invites', defaultMessage: 'Grant more invites' })}
          </Button>
        </div>
      ) : review ? (
        <>
          <h3 className="font-medium">{t({ id: 'eunha.invite_grant_form.review_grant', defaultMessage: 'Review grant' })}</h3>
          <dl className="grid grid-cols-1 gap-2 text-sm sm:grid-cols-2">
            <dt className="text-muted-foreground">{t({ id: 'eunha.invite_grant_form.recipients', defaultMessage: 'Recipients' })}</dt>
            <dd>
              {recipient === 'everyone'
                ? t({ id: 'eunha.invite_grant_form.all_local_users_recipients', defaultMessage: 'All local users ({recipients})' }, { recipients })
                : `@${selected?.acct}`}
            </dd>
            <dt className="text-muted-foreground">{t({ id: 'eunha.invite_grant_form.people_each_can_invite', defaultMessage: 'People each can invite' })}</dt>
            <dd>{number}</dd>
            <dt className="text-muted-foreground">{t({ id: 'eunha.invite_grant_form.links_to_create', defaultMessage: 'Links to create' })}</dt>
            <dd>
              {t({ id: 'eunha.invite_grant_form.single_use_links', defaultMessage: '{count} single-use links' }, { count: number * recipients })}
            </dd>
            <dt className="text-muted-foreground">{t({ id: 'eunha.invite_grant_form.expires_after', defaultMessage: 'Expires after' })}</dt>
            <dd>{EXPIRIES[expiry as keyof typeof EXPIRIES]}</dd>
            <dt className="text-muted-foreground">{t({ id: 'eunha.invite_grant_form.admission', defaultMessage: 'Admission' })}</dt>
            <dd>{t({ id: 'eunha.invite_grant_form.no_staff_approval_needed_email_confirmation', defaultMessage: 'No staff approval needed. Email confirmation required.' })}</dd>
          </dl>
          <p className="text-muted-foreground text-xs">
            {t({ id: 'eunha.invite_grant_form.existing_links_and_member_roles_stay', defaultMessage: 'Existing links and member roles stay unchanged. Bulk grants include eligible local users at submission time; future users are not included.' })}
          </p>
          <div className="flex flex-wrap gap-2">
            <Button
              variant="outline"
              disabled={pending}
              onClick={() => setReview(false)}
            >
              {t({ id: 'column_back_button.label', defaultMessage: 'Back' })}
            </Button>
            <Button disabled={!ready || pending} onClick={submit}>
              {pending
                ? t({ id: 'eunha.invite_grant_form.granting', defaultMessage: 'Granting…' })
                : t({ id: 'eunha.invite_grant_form.grant', defaultMessage: 'Grant {count} invites' }, { count: number * recipients })}
            </Button>
          </div>
        </>
      ) : (
        <>
          {!members && !error && (
            <AccountListSkeleton count={3} />
          )}
          <div className="space-y-1">
            <Label htmlFor="grant-search">{t({ id: 'eunha.invite_grant_form.search_members', defaultMessage: 'Search members' })}</Label>
            <Input
              id="grant-search"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={t({ id: 'mastodon.admin.accounts.username', defaultMessage: 'Username' })}
            />
          </div>
          <ChoiceSelect
            label={t({ id: 'eunha.invite_grant_form.recipient', defaultMessage: 'Recipient' })}
            items={visibleItems}
            value={recipient}
            onChange={setRecipient}
            className="w-full"
          />
          <p className="text-muted-foreground text-xs">
            {t({ id: 'eunha.invite_grant_form.all_local_users_includes_confirmed_approved', defaultMessage: 'All local users includes confirmed, approved, functional members and staff. Remote and unavailable accounts are excluded.' })}
          </p>
          <div className="grid gap-3 sm:grid-cols-2">
            <div className="space-y-1">
              <Label htmlFor="grant-count">{t({ id: 'eunha.invite_grant_form.people_each_member_can_invite', defaultMessage: 'People each member can invite' })}</Label>
              <Input
                id="grant-count"
                type="number"
                min={1}
                max={25}
                step={1}
                value={count}
                onChange={(e) => setCount(e.target.value)}
              />
              <p className="text-muted-foreground text-xs">
                {t({ id: 'eunha.invite_grant_form.one_single_use_link_per_person', defaultMessage: 'One single-use link per person. Choose 1–25.' })}
              </p>
            </div>
            <ChoiceSelect
              label={t({ id: 'eunha.invite_grant_form.links_expire_after', defaultMessage: 'Links expire after' })}
              items={EXPIRIES}
              value={expiry}
              onChange={setExpiry}
              className="w-full"
            />
          </div>
          <p className="text-muted-foreground text-sm">
            {t({ id: 'eunha.invite_grant_form.no_staff_approval_needed_for_these', defaultMessage: 'No staff approval needed for these invites. Email confirmation is still required.' })}
          </p>
          {ready && (
            <p role="status" className="text-sm">
              {selected
                ? t(
                    {
                      id: 'eunha.invite_grant_form.summary_one',
                      defaultMessage: 'Create {links} single-use links for {recipients, plural, one {# local user} other {# local users}} (@{acct}).',
                    },
                    { links: number * recipients, recipients, acct: selected.acct },
                  )
                : t(
                    {
                      id: 'eunha.invite_grant_form.summary',
                      defaultMessage: 'Create {links} single-use links for {recipients, plural, one {# local user} other {# local users}}.',
                    },
                    { links: number * recipients, recipients },
                  )}
            </p>
          )}
          <Button disabled={!ready} onClick={() => setReview(true)}>
            {t({ id: 'eunha.invite_grant_form.review_grant', defaultMessage: 'Review grant' })}
          </Button>
        </>
      )}
    </section>
  )
}
