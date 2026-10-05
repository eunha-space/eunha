import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { Copy, Trash2 } from 'lucide-react'
import { toast } from 'sonner'
import {
  createInvite,
  deleteInvite,
  getInvites,
  INVITES_CHANGED,
  type Invite,
} from '../eunha-api.ts'
import { getInvitePermissions } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import {
  inviteDate,
  inviteExpiry,
  inviteStatus,
  availableInviteCount,
} from '../lib/invites.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select.tsx'

const MAX_USES = {
  '0': 'Unlimited uses',
  '1': '1 use',
  '5': '5 uses',
  '10': '10 uses',
  '25': '25 uses',
  '50': '50 uses',
  '100': '100 uses',
}
const EXPIRES_IN = {
  '0': 'Never expires',
  '1800': '30 minutes',
  '3600': '1 hour',
  '21600': '6 hours',
  '43200': '12 hours',
  '86400': '1 day',
  '604800': '1 week',
}
function approval(invite: Invite) {
  return invite.bypass_approval
    ? 'No staff approval needed'
    : 'Follows instance approval rules'
}
function grantLabel(invite: Invite) {
  return `${invite.grant?.granted_by ? `From @${invite.grant.granted_by}` : 'From staff'} · ${inviteDate(invite.grant?.created_at ?? invite.created_at).toLocaleString()}`
}
function InviteRow({
  invite,
  pending,
  copied,
  onCopy,
  onRevoke,
}: {
  invite: Invite
  pending: boolean
  copied: boolean
  onCopy: (invite: Invite) => void
  onRevoke: (id: string) => void
}) {
  const status = inviteStatus(invite)
  return (
    <div className="space-y-2 rounded-lg border p-3">
      <div className="flex flex-wrap items-center gap-2">
        <Input
          aria-label="Invite link"
          readOnly
          value={invite.url}
          className="min-w-0 flex-1 font-mono text-xs"
        />
        <Button
          size="sm"
          variant="secondary"
          onClick={() => onCopy(invite)}
          disabled={status !== 'Available'}
        >
          <Copy /> Copy
        </Button>
        {status === 'Available' && (
          <Button
            disabled={pending}
            size="sm"
            variant="ghost"
            aria-label="Revoke invite"
            onClick={() => onRevoke(invite.id)}
          >
            <Trash2 />
          </Button>
        )}
      </div>
      <div className="text-muted-foreground flex flex-wrap gap-x-3 gap-y-1 text-xs">
        <Badge variant="outline">
          {status === 'Used' ? 'Fully used' : status}
        </Badge>
        <span>
          {invite.uses}
          {invite.max_uses != null ? ` / ${invite.max_uses}` : ''} used
        </span>
        <span>{inviteExpiry(invite)}</span>
        <span>{approval(invite)}</span>
        {copied && status === 'Available' && (
          <span>Copied this visit · still available</span>
        )}
        {invite.autofollow && <span>New members follow you</span>}
      </div>
    </div>
  )
}

export default function Invites() {
  const token = getToken()
  const [invites, setInvites] = useState<Invite[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [perms, setPerms] = useState({ canInvite: false, canGrant: false })
  const [permissionError, setPermissionError] = useState<string | null>(null)
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [copied, setCopied] = useState<Set<string>>(new Set())
  const [revoking, setRevoking] = useState<Set<string>>(new Set())
  const [status, setStatus] = useState('Available')
  const [maxUses, setMaxUses] = useState('1')
  const [expiresIn, setExpiresIn] = useState('0')
  const [autofollow, setAutofollow] = useState(false)
  const [creating, setCreating] = useState(false)
  const [, tick] = useState(0)
  const [retry, setRetry] = useState(0)
  useEffect(() => {
    if (!token) return
    let cancelled = false
    let sequence = 0
    setInvites(null)
    setCopied(new Set())
    const load = async () => {
      const request = ++sequence
      try {
        const list = await getInvites(token)
        if (!cancelled && request === sequence) {
          setInvites(list)
          setError(null)
        }
      } catch {
        if (!cancelled && request === sequence)
          setError('Could not load invites. Please try again.')
      }
    }
    void load()
    getInvitePermissions(token)
      .then((p) => {
        if (!cancelled) setPerms(p)
      })
      .catch(() => {
        if (!cancelled)
          setPermissionError(
            'Could not load invite permissions. Reload this page to try again.',
          )
      })
    const timer = window.setInterval(load, 60_000)
    window.addEventListener(INVITES_CHANGED, load)
    window.addEventListener('focus', load)
    return () => {
      cancelled = true
      window.clearInterval(timer)
      window.removeEventListener(INVITES_CHANGED, load)
      window.removeEventListener('focus', load)
    }
  }, [token, retry])
  useEffect(() => {
    const times = (invites ?? [])
      .flatMap((i) =>
        i.expires_at ? [inviteDate(i.expires_at).getTime()] : [],
      )
      .filter((t) => t > Date.now())
    if (!times.length) return
    const timer = window.setTimeout(
      () => tick((n) => n + 1),
      Math.min(2_147_483_647, Math.max(1, Math.min(...times) - Date.now() + 1)),
    )
    return () => window.clearTimeout(timer)
  })
  const usable = (invites ?? [])
    .filter((i) => inviteStatus(i) === 'Available')
    .sort(
      (a, b) =>
        (a.expires_at ? inviteDate(a.expires_at).getTime() : Infinity) -
        (b.expires_at ? inviteDate(b.expires_at).getTime() : Infinity),
    )
  const selected = usable.find((i) => i.id === selectedId) ?? usable[0]
  useEffect(() => {
    if (selected && selected.id !== selectedId) setSelectedId(selected.id)
  }, [selected?.id, selectedId])
  const allowance = availableInviteCount(invites ?? [])
  const historyGroups = new Map<string, Invite[]>()
  for (const invite of invites ?? [])
    if (
      (inviteStatus(invite) === 'Used'
        ? 'Fully used'
        : inviteStatus(invite)) === status
    ) {
      const key = invite.grant?.id ?? 'own'
      historyGroups.set(key, [...(historyGroups.get(key) ?? []), invite])
    }
  const copy = async (invite: Invite) => {
    if (inviteStatus(invite) !== 'Available') return
    try {
      await navigator.clipboard.writeText(invite.url)
      setCopied((c) => new Set(c).add(invite.id))
      toast.success(
        invite.max_uses === 1
          ? 'Invite link copied. Send it to one person.'
          : 'Invite link copied',
      )
    } catch {
      toast.error('Could not copy link')
    }
  }
  const next = () => {
    if (!selected) return
    const other = usable.filter((i) => i.id !== selected.id)
    setSelectedId(
      (other.find((i) => !copied.has(i.id)) ?? other[0])?.id ?? selected.id,
    )
  }
  const create = async () => {
    if (!token || creating) return
    setCreating(true)
    try {
      const invite = await createInvite(token, {
        max_uses: maxUses === '0' ? undefined : Number(maxUses),
        expires_in: expiresIn === '0' ? undefined : Number(expiresIn),
        autofollow,
      })
      setInvites((prev) => [
        invite,
        ...(prev ?? []).filter((i) => i.id !== invite.id),
      ])
      toast.success('Invite link created')
    } catch {
      toast.error('Could not create invite')
    } finally {
      setCreating(false)
    }
  }
  const revoke = async (id: string) => {
    if (!token || revoking.has(id)) return
    setRevoking((s) => new Set(s).add(id))
    try {
      await deleteInvite(token, id)
      setInvites(
        (list) =>
          list?.map((i) =>
            i.id === id
              ? {
                  ...i,
                  expired: true,
                  valid_for_use: false,
                  expires_at: new Date().toISOString(),
                }
              : i,
          ) ?? null,
      )
      toast.success('Invite revoked')
    } catch {
      toast.error('Could not revoke invite')
    } finally {
      setRevoking((s) => {
        const n = new Set(s)
        n.delete(id)
        return n
      })
    }
  }
  return (
    <div className="page-frame">
      <TopBar />
      <div className="mb-5 flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-lg font-bold">Your invites</h1>
          <p className="text-muted-foreground text-sm">
            Bring someone you would like to see here.
          </p>
        </div>
        <Link to="/invite-tree" className="text-sm underline">
          Invite tree
        </Link>
      </div>
      {!token ? (
        <div className="space-y-2">
          <p className="text-muted-foreground text-sm">
            Sign in to view your invites.
          </p>
          <Button onClick={() => beginLogin()}>Sign in</Button>
        </div>
      ) : (
        <>
          {permissionError && (
            <p role="alert" className="text-destructive mb-3 text-sm">
              {permissionError}
            </p>
          )}
          {perms.canGrant && (
            <Button
              className="mb-4"
              variant="outline"
              render={<Link to="/admin/invites" />}
            >
              Grant invites
            </Button>
          )}
          {error && (
            <p role="alert" className="text-destructive mb-3 text-sm">
              {error}{' '}
              <Button
                size="sm"
                variant="outline"
                onClick={() => setRetry((n) => n + 1)}
              >
                Try again
              </Button>
            </p>
          )}
          {invites === null && !error && (
            <p role="status" className="text-muted-foreground text-sm">
              Loading invites…
            </p>
          )}
          {invites !== null && (
            <section
              className="mb-6 space-y-3 rounded-lg border p-4"
              aria-label="Share an invite"
            >
              <h2 className="text-xl font-semibold">
                {allowance
                  ? `Invite ${allowance} ${allowance === 1 ? 'person' : 'people'}`
                  : usable.length
                    ? `${usable.length} invite ${usable.length === 1 ? 'link' : 'links'} available`
                    : 'No invites available'}
              </h2>
              {selected ? (
                <>
                  <p className="text-muted-foreground text-sm">
                    {selected.max_uses === 1
                      ? 'Each link admits one person.'
                      : selected.max_uses === null
                        ? 'This link allows unlimited signups.'
                        : `This link has ${selected.max_uses - selected.uses} uses remaining.`}
                    {perms.canInvite ? ' You can also create more links.' : ''}
                  </p>
                  <div className="text-muted-foreground flex flex-wrap justify-between gap-2 text-xs">
                    <span>{inviteExpiry(selected)}</span>
                  </div>
                  <p className="text-muted-foreground text-xs">
                    {approval(selected)} · Email confirmation required
                  </p>
                  <Input
                    aria-label="Selected invite link"
                    readOnly
                    value={selected.url}
                    className="font-mono text-xs"
                  />
                  <div className="flex flex-wrap gap-2">
                    <Button disabled={!!error} onClick={() => copy(selected)}>
                      <Copy />
                      {copied.has(selected.id)
                        ? 'Copy this link again'
                        : 'Copy invite link'}
                    </Button>
                    <Button
                      variant="outline"
                      disabled={usable.length < 2 || !!error}
                      onClick={next}
                    >
                      Choose another link
                    </Button>
                  </div>
                  <p className="text-muted-foreground text-xs">
                    {copied.has(selected.id) ? 'Copied this visit. ' : ''}
                    {selected.max_uses === 1
                      ? 'Send this link to one person. Choose another link for someone else.'
                      : 'Share this link within its remaining uses.'}{' '}
                    Copying does not consume a use.
                  </p>
                </>
              ) : (
                <p className="text-muted-foreground text-sm">
                  {perms.canInvite
                    ? 'Create an invite link below.'
                    : 'Staff distribute invites on this instance. Check back when you receive more.'}
                </p>
              )}
            </section>
          )}
          {perms.canInvite && (
            <details className="mb-6 rounded-lg border p-4">
              <summary className="cursor-pointer font-medium">
                Create invite link
              </summary>
              <div className="mt-4 space-y-3">
                <div className="grid gap-3 sm:grid-cols-2">
                  <div className="space-y-1">
                    <Label>Uses</Label>
                    <Select
                      items={MAX_USES}
                      value={maxUses}
                      onValueChange={(v) => setMaxUses(v ?? '0')}
                    >
                      <SelectTrigger
                        className="w-full"
                        aria-label="Maximum uses"
                      >
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectGroup>
                          {Object.entries(MAX_USES).map(([value, label]) => (
                            <SelectItem key={value} value={value}>
                              {label}
                            </SelectItem>
                          ))}
                        </SelectGroup>
                      </SelectContent>
                    </Select>
                  </div>
                  <div className="space-y-1">
                    <Label>Expiry</Label>
                    <Select
                      items={EXPIRES_IN}
                      value={expiresIn}
                      onValueChange={(v) => setExpiresIn(v ?? '0')}
                    >
                      <SelectTrigger className="w-full" aria-label="Expiry">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectGroup>
                          {Object.entries(EXPIRES_IN).map(([value, label]) => (
                            <SelectItem key={value} value={value}>
                              {label}
                            </SelectItem>
                          ))}
                        </SelectGroup>
                      </SelectContent>
                    </Select>
                  </div>
                </div>
                <label className="flex items-center gap-2 text-sm">
                  <Switch
                    checked={autofollow}
                    onCheckedChange={setAutofollow}
                  />
                  New members auto-follow me
                </label>
                <Button onClick={create} disabled={creating}>
                  {creating ? 'Creating…' : 'Create invite link'}
                </Button>
              </div>
            </details>
          )}

          {invites && invites.length > 0 && (
            <details className="mb-6">
              <summary className="cursor-pointer py-2 text-sm font-medium">
                Manage links{' '}
                <span className="text-muted-foreground ml-2 font-normal">
                  {usable.length} available
                </span>
              </summary>
              <div
                className="my-3 flex flex-wrap gap-2"
                role="group"
                aria-label="Link status"
              >
                {['Available', 'Fully used', 'Expired', 'Unavailable'].map(
                  (s) => (
                    <Button
                      key={s}
                      size="sm"
                      variant={status === s ? 'default' : 'outline'}
                      aria-pressed={status === s}
                      onClick={() => setStatus(s)}
                    >
                      {s}
                    </Button>
                  ),
                )}
              </div>
              <div className="space-y-4">
                {[...historyGroups].map(([id, list]) => (
                  <section key={id} className="space-y-2">
                    <h3 className="text-muted-foreground text-xs">
                      {list[0].grant
                        ? grantLabel(list[0])
                        : 'Other invite links'}
                    </h3>
                    {list.map((invite) => (
                      <InviteRow
                        key={invite.id}
                        invite={invite}
                        copied={copied.has(invite.id)}
                        onCopy={copy}
                        onRevoke={revoke}
                        pending={revoking.has(invite.id)}
                      />
                    ))}
                  </section>
                ))}
                {historyGroups.size === 0 && (
                  <p className="text-muted-foreground text-sm">
                    No {status.toLowerCase()} links.
                  </p>
                )}
              </div>
            </details>
          )}
        </>
      )}
    </div>
  )
}
