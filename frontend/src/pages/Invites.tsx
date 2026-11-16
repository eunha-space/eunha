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
  inviteStatusLabel,
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
import { SectionSkeleton } from '@/components/skeletons.tsx'
import { currentLocale, t } from '@/i18n.tsx'

const MAX_USES = {
  get '0'() { return t({ id: 'eunha.invites.unlimited_uses', defaultMessage: 'Unlimited uses' }) },
  '1': '1 use',
  '5': '5 uses',
  '10': '10 uses',
  '25': '25 uses',
  '50': '50 uses',
  '100': '100 uses',
}
const EXPIRES_IN = {
  get '0'() { return t({ id: 'eunha.invites.never_expires', defaultMessage: 'Never expires' }) },
  '1800': '30 minutes',
  '3600': '1 hour',
  '21600': '6 hours',
  '43200': '12 hours',
  '86400': '1 day',
  '604800': '1 week',
}
function approval(invite: Invite) {
  return invite.bypass_approval
    ? t({ id: 'eunha.invites.no_staff_approval_needed', defaultMessage: 'No staff approval needed' })
    : t({ id: 'eunha.invites.follows_instance_approval_rules', defaultMessage: 'Follows instance approval rules' })
}
function grantLabel(invite: Invite) {
  return `${invite.grant?.granted_by ? t({ id: 'eunha.invites.from', defaultMessage: 'From @{granted_by}' }, { granted_by: invite.grant.granted_by }) : t({ id: 'eunha.invites.from_staff', defaultMessage: 'From staff' })} · ${inviteDate(invite.grant?.created_at ?? invite.created_at).toLocaleString(currentLocale())}`
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
          aria-label={t({ id: 'eunha.invites.invite_link', defaultMessage: 'Invite link' })}
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
          <Copy /> {t({ id: 'mastodon.generic.copy', defaultMessage: 'Copy' })}
        </Button>
        {status === 'Available' && (
          <Button
            disabled={pending}
            size="sm"
            variant="ghost"
            aria-label={t({ id: 'eunha.invites.revoke_invite', defaultMessage: 'Revoke invite' })}
            onClick={() => onRevoke(invite.id)}
          >
            <Trash2 />
          </Button>
        )}
      </div>
      <div className="text-muted-foreground flex flex-wrap gap-x-3 gap-y-1 text-xs">
        <Badge variant="outline">
          {status === 'Used' ? t({ id: 'eunha.invites.fully_used', defaultMessage: 'Fully used' }) : inviteStatusLabel(status)}
        </Badge>
        <span>
          {invite.max_uses != null
            ? t({ id: 'eunha.invites.uses_of_max', defaultMessage: '{uses} / {max} used' }, { uses: invite.uses, max: invite.max_uses })
            : t({ id: 'eunha.invites.uses_used', defaultMessage: '{uses} used' }, { uses: invite.uses })}
        </span>
        <span>{inviteExpiry(invite)}</span>
        <span>{approval(invite)}</span>
        {copied && status === 'Available' && (
          <span>{t({ id: 'eunha.invites.copied_this_visit_still_available', defaultMessage: 'Copied this visit · still available' })}</span>
        )}
        {invite.autofollow && <span>{t({ id: 'eunha.invites.new_members_follow_you', defaultMessage: 'New members follow you' })}</span>}
      </div>
      <JoinedThrough invite={invite} />
    </div>
  )
}

/** Who came through a link, as the invite tree would show them. */
function JoinedThrough({ invite }: { invite: Invite }) {
  const { accounts, hidden, pending } = invite.joined ?? { accounts: [], hidden: 0, pending: 0 }
  if (!accounts.length && !hidden && !pending) return null
  return (
    <p className="text-muted-foreground text-xs">
      {(accounts.length > 0 || hidden > 0) && <>
        {t({ id: 'eunha.invites.joined', defaultMessage: 'Joined:' })}{' '}
        {accounts.map((account, i) => (
          <span key={account.id}>
            {i > 0 && ', '}
            <Link to={`/@${account.acct}`}>@{account.acct}</Link>
          </span>
        ))}
        {hidden > 0 && (accounts.length ? ', ' : '') + t({ id: 'eunha.invites.hidden_members', defaultMessage: '{count, plural, one {# member who hides} other {# members who hide}} their network' }, { count: hidden })}
      </>}
      {pending > 0 && (accounts.length || hidden ? ' · ' : '') + t({ id: 'eunha.invites.pending_members', defaultMessage: '{count} not finished joining (awaiting confirmation or approval)' }, { count: pending })}
    </p>
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
  const [status, setStatus] = useState('Available') // i18n-ignore: a filter value
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
          setError(t({ id: 'eunha.invites.could_not_load_invites_please_try', defaultMessage: 'Could not load invites. Please try again.' }))
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
            t({ id: 'eunha.invites.could_not_load_permissions', defaultMessage: 'Could not load invite permissions. Reload this page to try again.' }),
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
      // i18n-ignore: the filter's value, not its text.
      (inviteStatus(invite) === 'Used' ? 'Fully used' : inviteStatus(invite)) === status
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
          ? t({ id: 'eunha.invites.invite_link_copied_send_it_to', defaultMessage: 'Invite link copied. Send it to one person.' })
          : t({ id: 'eunha.invites.invite_link_copied', defaultMessage: 'Invite link copied' }),
      )
    } catch {
      toast.error(t({ id: 'eunha.invites.could_not_copy_link', defaultMessage: 'Could not copy link' }))
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
      toast.success(t({ id: 'eunha.invites.invite_link_created', defaultMessage: 'Invite link created' }))
    } catch {
      toast.error(t({ id: 'eunha.invites.could_not_create_invite', defaultMessage: 'Could not create invite' }))
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
      toast.success(t({ id: 'eunha.invites.invite_revoked', defaultMessage: 'Invite revoked' }))
    } catch {
      toast.error(t({ id: 'eunha.invites.could_not_revoke_invite', defaultMessage: 'Could not revoke invite' }))
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
          <h1 className="text-lg font-bold">{t({ id: 'eunha.invites.your_invites', defaultMessage: 'Your invites' })}</h1>
          <p className="text-muted-foreground text-sm">
            {t({ id: 'eunha.invites.bring_someone_you_would_like_to', defaultMessage: 'Bring someone you would like to see here.' })}
          </p>
        </div>
        <Link to="/invite-tree" className="text-sm underline">
          {t({ id: 'eunha.invites.invite_tree', defaultMessage: 'Invite tree' })}
        </Link>
      </div>
      {!token ? (
        <div className="space-y-2">
          <p className="text-muted-foreground text-sm">
            {t({ id: 'eunha.invites.sign_in_to_view_your_invites', defaultMessage: 'Sign in to view your invites.' })}
          </p>
          <Button onClick={() => beginLogin()}>{t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}</Button>
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
              {t({ id: 'eunha.invites.grant_invites', defaultMessage: 'Grant invites' })}
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
                {t({ id: 'bundle_column_error.retry', defaultMessage: 'Try again' })}
              </Button>
            </p>
          )}
          {invites === null && !error && (
            <SectionSkeleton rows={3} />
          )}
          {invites !== null && (
            <section
              className="mb-6 space-y-3 rounded-lg border p-4"
              aria-label={t({ id: 'eunha.invites.share_an_invite', defaultMessage: 'Share an invite' })}
            >
              <h2 className="text-xl font-semibold">
                {allowance
                  ? t({ id: 'eunha.invites.invite_people', defaultMessage: 'Invite {count, plural, one {# person} other {# people}}' }, { count: allowance })
                  : usable.length
                    ? t({ id: 'eunha.invites.links_available', defaultMessage: '{count, plural, one {# invite link} other {# invite links}} available' }, { count: usable.length })
                    : t({ id: 'eunha.invites.no_invites_available', defaultMessage: 'No invites available' })}
              </h2>
              {selected ? (
                <>
                  <p className="text-muted-foreground text-sm">
                    {selected.max_uses === 1
                      ? t({ id: 'eunha.invites.each_link_admits_one_person', defaultMessage: 'Each link admits one person.' })
                      : selected.max_uses === null
                        ? t({ id: 'eunha.invites.this_link_allows_unlimited_signups', defaultMessage: 'This link allows unlimited signups.' })
                        : t({ id: 'eunha.invites.uses_remaining', defaultMessage: 'This link has {count} uses remaining.' }, { count: selected.max_uses - selected.uses })}
                    {perms.canInvite ? t({ id: 'eunha.invites.you_can_also_create_more_links', defaultMessage: ' You can also create more links.' }) : ''}
                  </p>
                  <div className="text-muted-foreground flex flex-wrap justify-between gap-2 text-xs">
                    <span>{inviteExpiry(selected)}</span>
                  </div>
                  <p className="text-muted-foreground text-xs">
                    {approval(selected)} · {t({ id: 'eunha.invites.email_confirmation_required', defaultMessage: 'Email confirmation required' })}
                  </p>
                  <Input
                    aria-label={t({ id: 'eunha.invites.selected_invite_link', defaultMessage: 'Selected invite link' })}
                    readOnly
                    value={selected.url}
                    className="font-mono text-xs"
                  />
                  <div className="flex flex-wrap gap-2">
                    <Button disabled={!!error} onClick={() => copy(selected)}>
                      <Copy />
                      {copied.has(selected.id)
                        ? t({ id: 'eunha.invites.copy_this_link_again', defaultMessage: 'Copy this link again' })
                        : t({ id: 'eunha.invites.copy_invite_link', defaultMessage: 'Copy invite link' })}
                    </Button>
                    <Button
                      variant="outline"
                      disabled={usable.length < 2 || !!error}
                      onClick={next}
                    >
                      {t({ id: 'eunha.invites.choose_another_link', defaultMessage: 'Choose another link' })}
                    </Button>
                  </div>
                  <p className="text-muted-foreground text-xs">
                    {copied.has(selected.id) ? t({ id: 'eunha.invites.copied_this_visit', defaultMessage: 'Copied this visit. ' }) : ''}
                    {selected.max_uses === 1
                      ? t({ id: 'eunha.invites.send_this_link_to_one_person', defaultMessage: 'Send this link to one person. Choose another link for someone else.' })
                      : t({ id: 'eunha.invites.share_this_link_within_its_remaining', defaultMessage: 'Share this link within its remaining uses.' })}{' '}
                    {t({ id: 'eunha.invites.copying_does_not_consume', defaultMessage: 'Copying does not consume a use.' })}
                  </p>
                </>
              ) : (
                <p className="text-muted-foreground text-sm">
                  {perms.canInvite
                    ? t({ id: 'eunha.invites.create_an_invite_link_below', defaultMessage: 'Create an invite link below.' })
                    : t({ id: 'eunha.invites.staff_distribute_invites_on_this_instance', defaultMessage: 'Staff distribute invites on this instance. Check back when you receive more.' })}
                </p>
              )}
            </section>
          )}
          {perms.canInvite && (
            <details className="mb-6 rounded-lg border p-4">
              <summary className="cursor-pointer font-medium">
                {t({ id: 'eunha.invites.create_invite_link', defaultMessage: 'Create invite link' })}
              </summary>
              <div className="mt-4 space-y-3">
                <div className="grid gap-3 sm:grid-cols-2">
                  <div className="space-y-1">
                    <Label>{t({ id: 'mastodon.invites.table.uses', defaultMessage: 'Uses' })}</Label>
                    <Select
                      items={MAX_USES}
                      value={maxUses}
                      onValueChange={(v) => setMaxUses(v ?? '0')}
                    >
                      <SelectTrigger
                        className="w-full"
                        aria-label={t({ id: 'eunha.invites.maximum_uses', defaultMessage: 'Maximum uses' })}
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
                    <Label>{t({ id: 'eunha.invites.expiry', defaultMessage: 'Expiry' })}</Label>
                    <Select
                      items={EXPIRES_IN}
                      value={expiresIn}
                      onValueChange={(v) => setExpiresIn(v ?? '0')}
                    >
                      <SelectTrigger className="w-full" aria-label={t({ id: 'eunha.invites.expiry', defaultMessage: 'Expiry' })}>
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
                  {t({ id: 'eunha.invites.new_members_auto_follow_me', defaultMessage: 'New members auto-follow me' })}
                </label>
                <Button onClick={create} disabled={creating}>
                  {creating ? t({ id: 'eunha.common.creating', defaultMessage: 'Creating…' }) : t({ id: 'eunha.invites.create_invite_link', defaultMessage: 'Create invite link' })}
                </Button>
              </div>
            </details>
          )}

          {invites && invites.length > 0 && (
            <details className="mb-6">
              <summary className="cursor-pointer py-2 text-sm font-medium">
                {t({ id: 'eunha.invites.manage_links', defaultMessage: 'Manage links' })}{' '}
                <span className="text-muted-foreground ml-2 font-normal">
                  {t({ id: 'eunha.invites.count_available', defaultMessage: '{count} available' }, { count: usable.length })}
                </span>
              </summary>
              <div
                className="my-3 flex flex-wrap gap-2"
                role="group"
                aria-label={t({ id: 'eunha.invites.link_status', defaultMessage: 'Link status' })}
              >
                {/* i18n-ignore: values; the buttons show their labels. */}
                {(['Available', 'Fully used', 'Expired', 'Unavailable'] as const).map(
                  (s) => (
                    <Button
                      key={s}
                      size="sm"
                      variant={status === s ? 'default' : 'outline'}
                      aria-pressed={status === s}
                      onClick={() => setStatus(s)}
                    >
                      {s === 'Fully used'
                        ? t({ id: 'eunha.invites.fully_used', defaultMessage: 'Fully used' })
                        : inviteStatusLabel(s)}
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
                        : t({ id: 'eunha.invites.other_invite_links', defaultMessage: 'Other invite links' })}
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
                    {t(
                      { id: 'eunha.invites.no_links', defaultMessage: '{status, select, Used {No used links.} Expired {No expired links.} Unavailable {No unavailable links.} other {No available links.}}' },
                      { status },
                    )}
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
