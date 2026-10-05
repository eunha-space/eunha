import { useEffect, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { Copy, Trash2 } from 'lucide-react'
import { toast } from 'sonner'

import {
  createInvite,
  deleteInvite,
  getInviteTree,
  getInvites,
  grantInvites,
  type Invite,
  type InviteNode,
} from '../eunha-api.ts'
import { getInvitePermissions } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
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

// Mirrors Mastodon's MAX_USES_COUNTS / EXPIRATION_DURATIONS. "0" means
// unlimited / never and is omitted from the request.
const MAX_USES: Record<string, string> = {
  '0': 'Unlimited uses',
  '1': '1 use',
  '5': '5 uses',
  '10': '10 uses',
  '25': '25 uses',
  '50': '50 uses',
  '100': '100 uses',
}
// A granted code is one person by default — "three invites" should mean three
// people, not three links of unbounded reach. Unlimited is deliberately absent.
const USES_PER_CODE: Record<string, string> = {
  '1': '1 use each',
  '5': '5 uses each',
  '10': '10 uses each',
  '25': '25 uses each',
}
const COUNTS: Record<string, string> = {
  '1': '1 invite',
  '2': '2 invites',
  '3': '3 invites',
  '5': '5 invites',
  '10': '10 invites',
}
/** Base UI's Select takes `items` as value → label, for what the trigger shows. */
function grantTargets(
  members: { id: string; acct: string }[],
): Record<string, string> {
  const items: Record<string, string> = {
    everyone: `Everyone (${members.length} ${members.length === 1 ? 'member' : 'members'})`,
  }
  for (const m of members) items[m.id] = `@${m.acct}`
  return items
}

const EXPIRES_IN: Record<string, string> = {
  '0': 'Never expires',
  '1800': '30 minutes',
  '3600': '1 hour',
  '21600': '6 hours',
  '43200': '12 hours',
  '86400': '1 day',
  '604800': '1 week',
}

// The API serializes timestamps as naive UTC (no offset); tag them as UTC so the
// browser doesn't read them as local time.
function asUtc(s: string): string {
  return /[Z+]/.test(s) ? s : `${s}Z`
}

function expiryLabel(invite: Invite): string {
  if (!invite.expires_at) return 'Never expires'
  const when = new Date(asUtc(invite.expires_at))
  return when.getTime() < Date.now()
    ? `Expired ${when.toLocaleString()}`
    : `Expires ${when.toLocaleString()}`
}

function InviteRow({
  invite,
  onRevoke,
  pending,
}: {
  invite: Invite
  pending: boolean
  onRevoke: (id: string) => void
}) {
  const expired = invite.expired || (!!invite.expires_at && new Date(asUtc(invite.expires_at)).getTime() < Date.now())
  const usable = invite.valid_for_use && !expired
  const status = expired ? 'Expired' : invite.max_uses !== null && invite.uses >= invite.max_uses ? 'Fully used' : usable ? 'Available' : 'Unavailable'
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(invite.url)
      toast.success('Invite link copied')
    } catch {
      toast.error('Could not copy link')
    }
  }

  return (
    <div className="space-y-2 rounded-lg border p-3">
      <div className="flex items-center gap-2">
        <Input aria-label="Invite link" readOnly value={invite.url} className="min-w-0 font-mono text-xs" />
        <Button size="sm" variant="secondary" onClick={copy} disabled={!usable}>
          <Copy /> Copy
        </Button>
        {usable && <Button
          disabled={pending}
          size="sm"
          variant="ghost"
          aria-label="Revoke invite"
          onClick={() => onRevoke(invite.id)}
        >
          <Trash2 />
        </Button>}
      </div>
      <div className="text-muted-foreground flex flex-wrap gap-x-3 text-xs">
        <Badge variant="outline">{status}</Badge>
        <span>
          {invite.uses}
          {invite.max_uses != null ? ` / ${invite.max_uses}` : ''} used
        </span>
        <span>{expiryLabel(invite)}</span>
        {invite.autofollow && <span>New members follow you</span>}
        {invite.comment && <span>“{invite.comment}”</span>}
      </div>
    </div>
  )
}

export default function Invites() {
  const token = getToken()
  const [params] = useSearchParams()
  const [revoking, setRevoking] = useState<Set<string>>(new Set())
  const [permissionError, setPermissionError] = useState<string | null>(null)
  const [permissionsLoaded, setPermissionsLoaded] = useState(false)
  const [invites, setInvites] = useState<Invite[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  // Mastodon's `invite_users` and `manage_invites`, read from the role the
  // server reports rather than guessed at here: the form and the hand-out panel
  // appear for exactly the accounts the server would let use them.
  const [perms, setPerms] = useState({ canInvite: false, canGrant: false })
  const [maxUses, setMaxUses] = useState('0')
  const [expiresIn, setExpiresIn] = useState('0')
  const [autofollow, setAutofollow] = useState(false)
  const [comment, setComment] = useState('')
  const [creating, setCreating] = useState(false)

  // Hand-out panel (admins only).
  const [members, setMembers] = useState<{ id: string; acct: string }[]>([])
  const [grantTo, setGrantTo] = useState(params.get('grant_to') ?? '')
  const [memberQuery, setMemberQuery] = useState('')
  const [memberError, setMemberError] = useState<string | null>(null)
  const [membersLoaded, setMembersLoaded] = useState(false)
  const [memberReload, setMemberReload] = useState(0)
  const [grantCount, setGrantCount] = useState('1')
  const [grantUses, setGrantUses] = useState('1')
  const [grantExpiry, setGrantExpiry] = useState('0')
  const [grantNote, setGrantNote] = useState('')
  const [granting, setGranting] = useState(false)

  const reload = () => {
    if (!token) return
    getInvites(token)
      .then((list) => { setInvites(list); setError(null) })
      .catch((e) => setError(String(e)))
  }

  useEffect(() => {
    if (!token) return
    getInvites(token)
      .then((list) => { setInvites(list); setError(null) })
      .catch((e) => setError(String(e)))
    getInvitePermissions(token).then((permissions) => { setPerms(permissions); setPermissionsLoaded(true) })
      .catch(() => setPermissionError('Could not load invite permissions. Reload this page to try again.'))
  }, [token])

  // The invite tree is the member list any member may already read, so the
  // picker costs no new endpoint. Flattened and sorted by name.
  useEffect(() => {
    if (!token || !perms.canGrant) return
    setMembersLoaded(false)
    setMemberError(null)
    getInviteTree(token)
      .then((tree) => {
        const flat: { id: string; acct: string }[] = []
        const walk = (nodes: InviteNode[]) => {
          for (const n of nodes) {
            flat.push({ id: n.id, acct: n.acct })
            walk(n.children)
          }
        }
        walk(tree.roots)
        flat.sort((a, b) => a.acct.localeCompare(b.acct))
        setMembers(flat)
        setMembersLoaded(true)
      })
      .catch(() => setMemberError("Could not load members. Try again before handing out invites."))
  }, [token, perms.canGrant, memberReload])

  const grant = async () => {
    if (!token || !membersLoaded || !grantTo || (grantTo !== 'everyone' && !members.some(m => m.id === grantTo))) return
    setGranting(true)
    try {
      const result = await grantInvites(token, {
        account_id: grantTo === 'everyone' ? undefined : grantTo,
        count: Number(grantCount),
        max_uses: Number(grantUses),
        expires_in: grantExpiry === '0' ? undefined : Number(grantExpiry),
        comment: grantNote.trim() || undefined,
      })
      setGrantNote('')
      toast.success(
        `Handed out ${result.granted} invite${result.granted === 1 ? '' : 's'} ` +
          `to ${result.accounts} member${result.accounts === 1 ? '' : 's'}`,
      )
      // The grant may have included this account.
      reload()
    } catch {
      toast.error('Could not hand out invites')
    } finally {
      setGranting(false)
    }
  }

  const create = async () => {
    if (!token) return
    setCreating(true)
    try {
      const invite = await createInvite(token, {
        max_uses: maxUses === '0' ? undefined : Number(maxUses),
        expires_in: expiresIn === '0' ? undefined : Number(expiresIn),
        autofollow,
        comment: comment.trim() || undefined,
      })
      setInvites((prev) => [invite, ...(prev ?? [])])
      setComment('')
      toast.success('Invite link created')
    } catch {
      toast.error('Could not create invite')
    } finally {
      setCreating(false)
    }
  }

  const revoke = async (id: string) => {
    if (!token) return
    if (revoking.has(id)) return
    setRevoking((current) => new Set(current).add(id))
    try {
      await deleteInvite(token, id)
      setInvites((current) => current?.map((invite) => invite.id === id
        ? { ...invite, expired: true, valid_for_use: false, expires_at: new Date().toISOString() }
        : invite) ?? null)
      toast.success('Invite revoked')
    } catch {
      toast.error('Could not revoke invite')
    } finally {
      setRevoking((current) => { const next = new Set(current); next.delete(id); return next })
    }
  }
  const filteredMembers = members.filter(m => m.acct.toLowerCase().includes(memberQuery.toLowerCase()))
  const recipientCount = grantTo === 'everyone' ? members.length : grantTo ? 1 : 0
  const selectedMember = members.find(m => m.id === grantTo)
  const grantReady = membersLoaded && !memberError && recipientCount > 0 && (grantTo === 'everyone' || !!selectedMember)

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-1 text-lg font-bold">Invites</h1>
      <p className="text-muted-foreground mb-4 text-sm">
        {!permissionsLoaded && token ? 'Loading invite permissions…' : perms.canInvite
          ? 'Create a link to invite people to this instance.'
          : 'Invites to this instance are handed out by its admins. Any that are yours are below.'}
      </p>

      {!token ? (
        <div className="space-y-2">
          <p className="text-muted-foreground text-sm">
            Sign in to create invite links.
          </p>
          <Button size="sm" onClick={() => beginLogin()}>
            Sign in
          </Button>
        </div>
      ) : (
        <>
          <Link to="/invite-tree" className="mb-4 inline-block text-sm underline">View invite tree</Link>
          {permissionError && <p role="alert" className="text-destructive mb-3 text-sm">{permissionError}</p>}
          {perms.canGrant && (
            <div className="bg-muted/30 mb-6 space-y-3 rounded-lg border p-4">
              <div>
                <h2 className="text-sm font-semibold">Hand out invites</h2>
                <p className="text-muted-foreground text-xs">
                  Creates codes in someone else's name. They appear on that
                  member's own invite page, and whoever signs up through one
                  joins the tree under them. Approval bypass follows that
                  member's permissions, including when staff hand out the codes.
                </p>
              </div>
              {!membersLoaded && !memberError && <p role="status" className="text-sm">Loading members…</p>}
              {memberError && <div role="alert" className="text-destructive text-sm">{memberError} <Button variant="outline" size="sm" onClick={() => setMemberReload(n => n + 1)}>Try again</Button></div>}
              <div className="space-y-1">
                <Label htmlFor="member-search">Search members</Label>
                <Input id="member-search" value={memberQuery} onChange={e => setMemberQuery(e.target.value)} placeholder="Username" />
              </div>
              <div className="grid gap-3 sm:grid-cols-2">
                <div className="space-y-1">
                  <Label>Who</Label>
                  <Select
                    items={grantTargets(members)}
                    value={grantTo}
                    disabled={!membersLoaded || !!memberError || granting}
                    onValueChange={(v) => setGrantTo(v ?? '')}
                  >
                    <SelectTrigger className="w-full" aria-label="Recipient">
                      <SelectValue placeholder="Choose a recipient" />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectGroup>
                        <SelectItem value="everyone">
                          Everyone ({members.length}{' '}
                          {members.length === 1 ? 'member' : 'members'})
                        </SelectItem>
                        {filteredMembers.map((m) => (
                          <SelectItem key={m.id} value={m.id}>
                            @{m.acct}
                          </SelectItem>
                        ))}
                      </SelectGroup>
                    </SelectContent>
                  </Select>
                </div>
                <div className="space-y-1">
                  <Label>How many each</Label>
                  <Select
                    items={COUNTS}
                    value={grantCount}
                    onValueChange={(v) => setGrantCount(v ?? '1')}
                  >
                    <SelectTrigger className="w-full" aria-label="How many each">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectGroup>
                        {Object.entries(COUNTS).map(([value, label]) => (
                          <SelectItem key={value} value={value}>
                            {label}
                          </SelectItem>
                        ))}
                      </SelectGroup>
                    </SelectContent>
                  </Select>
                </div>
                <div className="space-y-1">
                  <Label>Uses</Label>
                  <Select
                    items={USES_PER_CODE}
                    value={grantUses}
                    onValueChange={(v) => setGrantUses(v ?? '1')}
                  >
                    <SelectTrigger className="w-full" aria-label="Uses per code">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectGroup>
                        {Object.entries(USES_PER_CODE).map(([value, label]) => (
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
                    value={grantExpiry}
                    onValueChange={(v) => setGrantExpiry(v ?? '0')}
                  >
                    <SelectTrigger className="w-full" aria-label="Grant expiry">
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
              <div className="space-y-1">
                <Label htmlFor="grant-note">Note (optional)</Label>
                <Input
                  id="grant-note"
                  value={grantNote}
                  maxLength={420}
                  placeholder="Shown to the member on their invite"
                  onChange={(e) => setGrantNote(e.target.value)}
                />
              </div>
              {grantReady && <p className="text-sm" role="status">
                Create {recipientCount * Number(grantCount)} {recipientCount * Number(grantCount) === 1 ? 'link' : 'links'} for {recipientCount} {recipientCount === 1 ? 'member' : 'members'}{selectedMember ? ` (@${selectedMember.acct})` : ''}; each link admits {grantUses} {grantUses === '1' ? 'person' : 'people'}.
              </p>}
              <Button onClick={grant} disabled={granting || !grantReady}>
                {granting ? 'Handing out…' : 'Hand out invites'}
              </Button>
            </div>
          )}

          {perms.canInvite && (
          <div className="mb-6 space-y-3 rounded-lg border p-4">
            <div className="grid gap-3 sm:grid-cols-2">
              <div className="space-y-1">
                <Label>Uses</Label>
                <Select
                  items={MAX_USES}
                  value={maxUses}
                  onValueChange={(v) => setMaxUses(v ?? '0')}
                >
                  <SelectTrigger className="w-full" aria-label="Maximum uses">
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
            <div className="space-y-1">
              <Label htmlFor="invite-comment">Note (optional)</Label>
              <Input
                id="invite-comment"
                value={comment}
                maxLength={420}
                placeholder="What is this invite for?"
                onChange={(e) => setComment(e.target.value)}
              />
            </div>
            <label className="flex items-center gap-2 text-sm">
              <Switch checked={autofollow} onCheckedChange={setAutofollow} />
              New members auto-follow me
            </label>
            <Button onClick={create} disabled={creating}>
              {creating ? 'Creating…' : 'Create invite link'}
            </Button>
          </div>
          )}

          {error && <p className="text-destructive text-sm">{error}</p>}
          {invites === null && !error && (
            <p className="text-muted-foreground text-sm">Loading…</p>
          )}
          {invites?.length === 0 && (
            <p className="text-muted-foreground text-sm">No invites yet.</p>
          )}
          <div className="space-y-2">
            {invites?.map((invite) => (
              <InviteRow key={invite.id} invite={invite} onRevoke={revoke} pending={revoking.has(invite.id)} />
            ))}
          </div>
        </>
      )}
    </div>
  )
}
