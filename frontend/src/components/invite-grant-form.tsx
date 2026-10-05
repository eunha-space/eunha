import { useEffect, useState } from 'react'
import { useSearchParams } from 'react-router-dom'
import { toast } from 'sonner'
import { getGrantRecipients, grantInvites } from '../eunha-api.ts'
import { getToken } from '../auth.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { ChoiceSelect } from '@/components/admin/admin-common.tsx'

const EXPIRIES = { '604800': '7 days', '86400': '1 day', '0': 'Never' }

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
          setError('Could not load members. Try again before granting invites.')
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
    '': 'Choose a recipient',
    everyone: `All local users (${members?.length ?? 0})`,
    ...Object.fromEntries((members ?? []).map((m) => [m.id, `@${m.acct}`])),
  }
  const visibleItems = {
    '': 'Choose a recipient',
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
        account_id: recipient === 'everyone' ? undefined : recipient,
        count: number,
        max_uses: 1,
        expires_in: expiry === '0' ? undefined : Number(expiry),
      })
      setResult(r)
      setReview(false)
      onGranted?.()
      toast.success(`Granted ${r.granted} invites to ${r.accounts} local users`)
    } catch {
      setError('Could not grant invites. Please try again.')
    } finally {
      setPending(false)
    }
  }
  return (
    <section
      className="mb-6 space-y-4 rounded-lg border p-4"
      aria-label="Grant invites"
    >
      <div>
        <h2 className="text-base font-semibold">Grant invites</h2>
        <p className="text-muted-foreground text-sm">
          Give members people to bring along. Their invitees join the tree under
          them.
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
              Try again
            </Button>
          )}
        </p>
      )}
      {result ? (
        <div className="space-y-3" role="status">
          <h3 className="font-medium">
            {result.granted} invites granted to {result.accounts} local{' '}
            {result.accounts === 1 ? 'user' : 'users'}
          </h3>
          <p className="text-muted-foreground text-sm">
            Links are ready on each recipient’s Your invites page.
          </p>
          <Button
            variant="outline"
            onClick={() => {
              setResult(null)
              setRetry((n) => n + 1)
            }}
          >
            Grant more invites
          </Button>
        </div>
      ) : review ? (
        <>
          <h3 className="font-medium">Review grant</h3>
          <dl className="grid grid-cols-1 gap-2 text-sm sm:grid-cols-2">
            <dt className="text-muted-foreground">Recipients</dt>
            <dd>
              {recipient === 'everyone'
                ? `All local users (${recipients})`
                : `@${selected?.acct}`}
            </dd>
            <dt className="text-muted-foreground">People each can invite</dt>
            <dd>{number}</dd>
            <dt className="text-muted-foreground">Links to create</dt>
            <dd>{number * recipients} single-use links</dd>
            <dt className="text-muted-foreground">Expires after</dt>
            <dd>{EXPIRIES[expiry as keyof typeof EXPIRIES]}</dd>
            <dt className="text-muted-foreground">Admission</dt>
            <dd>No staff approval needed. Email confirmation required.</dd>
          </dl>
          <p className="text-muted-foreground text-xs">
            Existing links and member roles stay unchanged. Bulk grants include
            eligible local users at submission time; future users are not
            included.
          </p>
          <div className="flex flex-wrap gap-2">
            <Button
              variant="outline"
              disabled={pending}
              onClick={() => setReview(false)}
            >
              Back
            </Button>
            <Button disabled={!ready || pending} onClick={submit}>
              {pending ? 'Granting…' : `Grant ${number * recipients} invites`}
            </Button>
          </div>
        </>
      ) : (
        <>
          {!members && !error && (
            <p role="status" className="text-sm">
              Loading members…
            </p>
          )}
          <div className="space-y-1">
            <Label htmlFor="grant-search">Search members</Label>
            <Input
              id="grant-search"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Username"
            />
          </div>
          <ChoiceSelect
            label="Recipient"
            items={visibleItems}
            value={recipient}
            onChange={setRecipient}
            className="w-full"
          />
          <p className="text-muted-foreground text-xs">
            All local users includes confirmed, approved, functional members and
            staff. Remote and unavailable accounts are excluded.
          </p>
          <div className="grid gap-3 sm:grid-cols-2">
            <div className="space-y-1">
              <Label htmlFor="grant-count">People each member can invite</Label>
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
                One single-use link per person. Choose 1–25.
              </p>
            </div>
            <ChoiceSelect
              label="Links expire after"
              items={EXPIRIES}
              value={expiry}
              onChange={setExpiry}
              className="w-full"
            />
          </div>
          <p className="text-muted-foreground text-sm">
            No staff approval needed for these invites. Email confirmation is
            still required.
          </p>
          {ready && (
            <p role="status" className="text-sm">
              Create {number * recipients} single-use links for {recipients}{' '}
              local {recipients === 1 ? 'user' : 'users'}
              {selected ? ` (@${selected.acct})` : ''}.
            </p>
          )}
          <Button disabled={!ready} onClick={() => setReview(true)}>
            Review grant
          </Button>
        </>
      )}
    </section>
  )
}
