import { useCallback, useEffect, useMemo, useState, type KeyboardEvent } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { ChevronDown, ChevronRight, UserRound } from 'lucide-react'

import { getInviteTree, nestInviteTree, type InviteNode, type InviteTree } from '../eunha-api.ts'
import { getInvitePermissions } from '../api.ts'
import { beginLogin, getSavedAccounts, getToken } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { useAnimatedImage } from '@/hooks/use-animated-image.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'

const ROOT_REASONS = {
  no_recorded_inviter: 'No inviter recorded',
  lineage_unavailable: 'Earlier lineage is unavailable',
}

const PLACEHOLDER_NAMES = {
  former_member: 'A former member',
  hidden_member: 'A member who hides their network',
}

function nameOf(node: InviteNode): string {
  if (node.kind !== 'member') return PLACEHOLDER_NAMES[node.kind]
  return node.display_name || node.username || ''
}

function searchTree(nodes: InviteNode[], query: string): InviteNode[] {
  // Iterative post-order, so a long chain does not exhaust the stack.
  const result = new Map<InviteNode, InviteNode | null>()
  const stack: [InviteNode, boolean][] = nodes.map(node => [node, false])
  while (stack.length) {
    const [node, expanded] = stack.pop()!
    if (!expanded) {
      stack.push([node, true])
      for (const child of node.children) stack.push([child, false])
      continue
    }
    const children = node.children.map(child => result.get(child)).filter((c): c is InviteNode => !!c)
    const matches = node.kind === 'member'
      && `${node.acct ?? ''} ${node.display_name ?? ''}`.toLowerCase().includes(query)
    result.set(node, matches || children.length ? { ...node, children } : null)
  }
  return nodes.map(node => result.get(node)).filter((n): n is InviteNode => !!n)
}

/** Direct and total invitees of every node, counted without recursion. */
function countInvitees(roots: InviteNode[]): Map<string, { direct: number; total: number }> {
  const counts = new Map<string, { direct: number; total: number }>()
  const order: InviteNode[] = []
  const stack = [...roots]
  while (stack.length) {
    const node = stack.pop()!
    order.push(node)
    stack.push(...node.children)
  }
  for (let i = order.length - 1; i >= 0; i--) {
    const node = order[i]
    const total = node.children.reduce((sum, child) => sum + 1 + (counts.get(child.id)?.total ?? 0), 0)
    counts.set(node.id, { direct: node.children.length, total })
  }
  return counts
}

interface Visible { node: InviteNode; level: number; parent: string | null }

/** The rows on screen, in order: what arrow keys move between. */
function visibleRows(roots: InviteNode[], isOpen: (node: InviteNode) => boolean): Visible[] {
  const rows: Visible[] = []
  const stack: Visible[] = roots.map(node => ({ node, level: 1, parent: null })).reverse()
  while (stack.length) {
    const row = stack.pop()!
    rows.push(row)
    if (row.node.children.length && isOpen(row.node)) {
      for (let i = row.node.children.length - 1; i >= 0; i--)
        stack.push({ node: row.node.children[i], level: row.level + 1, parent: row.node.id })
    }
  }
  return rows
}

interface NodeProps {
  node: InviteNode
  level: number
  isOpen: (node: InviteNode) => boolean
  toggle: (id: string) => void
  counts: Map<string, { direct: number; total: number }>
  searching: boolean
  canGrant: boolean
  me: string | undefined
  active: string | null
  focused: string | null
  setActive: (id: string) => void
}

function TreeNode(props: NodeProps) {
  const { node, level, isOpen, toggle, counts, searching, canGrant, me, active, focused, setActive } = props
  const open = isOpen(node)
  const count = counts.get(node.id) ?? { direct: 0, total: 0 }
  const avatar = useAnimatedImage(node.avatar ?? '', node.avatar_static)
  const name = nameOf(node)
  const isMember = node.kind === 'member'
  return (
    <li role="treeitem" id={`invite-member-${node.id}`} aria-level={level}
      aria-expanded={node.children.length ? open : undefined}
      aria-selected={active === node.id}
      aria-label={isMember ? `${name} @${node.acct}` : name}
      tabIndex={active === node.id ? 0 : -1}
      onFocus={e => { if (e.target === e.currentTarget) setActive(node.id) }}
      className="rounded-lg outline-none focus-visible:ring-2 focus-visible:ring-primary">
      <div className={`rounded-lg p-2 ${focused === node.id ? 'bg-muted ring-primary ring-2' : ''}`}>
        <div className="flex min-w-0 items-center gap-2">
          {node.children.length > 0 ? <Button size="icon-sm" variant="ghost" tabIndex={-1}
            aria-label={`${open ? 'Collapse' : 'Expand'} ${isMember ? `@${node.acct}` : name}`}
            disabled={searching} onClick={() => toggle(node.id)}>
            {open ? <ChevronDown /> : <ChevronRight />}
          </Button> : <span className="size-7 shrink-0" />}
          {isMember ? <Link to={`/@${node.acct}`} tabIndex={-1} className="flex min-w-0 flex-1 items-center gap-2 no-underline">
            <Avatar className="size-8 shrink-0" {...avatar.hover}>
              <AvatarImage src={avatar.src} alt="" />
              <AvatarFallback>{name.slice(0, 1).toUpperCase()}</AvatarFallback>
            </Avatar>
            <div className="min-w-0">
              <div className="truncate font-medium">{name}{node.id === me && <span className="text-muted-foreground text-xs"> (you)</span>}</div>
              <div className="text-muted-foreground truncate text-sm">@{node.acct}</div>
            </div>
          </Link> : <div className="text-muted-foreground flex min-w-0 flex-1 items-center gap-2">
            <span className="bg-muted flex size-8 shrink-0 items-center justify-center rounded-full"><UserRound className="size-4" /></span>
            <span className="truncate italic">{name}</span>
          </div>}
        </div>
        <div className="text-muted-foreground ml-9 mt-1 flex flex-wrap gap-x-3 gap-y-1 text-xs">
          <span>{count.direct} direct {count.direct === 1 ? 'invitee' : 'invitees'}
            {count.total > count.direct && `, ${count.total} in all`}</span>
          {node.joined_at && <time dateTime={node.joined_at}>Joined {new Date(node.joined_at).toLocaleDateString()}</time>}
          {node.root_reason && <span>{ROOT_REASONS[node.root_reason]}</span>}
          {canGrant && isMember && <Link to={`/admin/invites?grant_to=${encodeURIComponent(node.id)}`} className="underline"
            tabIndex={active === node.id ? 0 : -1} aria-label={`Hand out invites to @${node.acct}`}>Hand out invites</Link>}
        </div>
      </div>
      {node.children.length > 0 && open && (
        <ul role="group" className={level < 5 ? "border-muted ml-3 border-l pl-1 sm:ml-5 sm:pl-2" : "border-muted border-l"}>
          {node.children.map(child => <TreeNode key={child.id} {...props} node={child} level={level + 1} />)}
        </ul>
      )}
    </li>
  )
}

export default function InviteTree() {
  const token = getToken()
  const me = getSavedAccounts().find(account => account.token === token)?.account.id
  // The view, the search and the member in focus live in the URL, so a link
  // can point at one member's place and Back undoes a change of view.
  const [params, setParams] = useSearchParams()
  const view: 'mine' | 'all' = params.get('view') === 'all' ? 'all' : 'mine'
  const query = params.get('q') ?? ''
  const focused = params.get('member')
  const update = useCallback((changes: Record<string, string | null>, replace = false) => {
    setParams(current => {
      const next = new URLSearchParams(current)
      for (const [key, value] of Object.entries(changes)) {
        if (value) next.set(key, value)
        else next.delete(key)
      }
      return next
    }, { replace })
  }, [setParams])

  const [trees, setTrees] = useState<{ mine?: InviteTree; all?: InviteTree }>({})
  const [error, setError] = useState<string | null>(null)
  const [canGrant, setCanGrant] = useState(false)
  const [expanded, setExpanded] = useState<Set<string> | null>(null)
  const [active, setActive] = useState<string | null>(null)
  const [reload, setReload] = useState(0)
  // Not in your own branch (no member row yet, or none in this view): show the
  // whole instance instead of an empty branch.
  const [noBranch, setNoBranch] = useState(false)
  const effectiveView = noBranch ? 'all' : view

  useEffect(() => {
    if (!token) return
    let live = true
    setError(null)
    getInviteTree(token, effectiveView === 'mine' ? 'branch' : 'all').then(result => {
      if (!live) return
      if (effectiveView === 'mine' && result.members.length === 0) { setNoBranch(true); return }
      setTrees(current => ({ ...current, [effectiveView]: result }))
    }).catch(() => { if (live) setError('Could not load the invite tree. Please try again.') })
    return () => { live = false }
  }, [token, effectiveView, reload])
  useEffect(() => {
    if (!token) return
    let live = true
    getInvitePermissions(token).then(perms => { if (live) setCanGrant(perms.canGrant) }).catch(() => {})
    return () => { live = false }
  }, [token])

  // While the other view loads, keep the last one on screen rather than
  // blanking the page; aria-busy says it is about to change.
  const loading = !trees[effectiveView]
  const tree = trees[effectiveView] ?? trees[effectiveView === 'all' ? 'mine' : 'all']
  const fullRoots = useMemo(() => tree ? nestInviteTree(tree.members) : [], [tree])
  const parents = useMemo(() => new Map(tree?.members.map(m => [m.id, m.parent_id]) ?? []), [tree])
  const lineOf = useCallback((id: string | null | undefined) => {
    const line: string[] = []
    const seen = new Set<string>()
    for (let at = id ?? null; at && !seen.has(at); at = parents.get(at) ?? null) {
      seen.add(at)
      line.push(at)
    }
    return line
  }, [parents])
  // Until the reader folds or unfolds something, open the way to the member
  // in focus, or to the viewer.
  const defaultOpen = useMemo(() => new Set(lineOf(focused ?? me)), [lineOf, focused, me])
  const openSet = expanded ?? defaultOpen
  const counts = useMemo(() => countInvitees(fullRoots), [fullRoots])
  const searching = query.trim().length > 0
  const isOpen = useCallback((node: InviteNode) => searching || openSet.has(node.id), [searching, openSet])
  const roots = useMemo(
    () => searching ? searchTree(fullRoots, query.trim().toLowerCase()) : fullRoots,
    [fullRoots, searching, query],
  )
  const rows = useMemo(() => visibleRows(roots, isOpen), [roots, isOpen])
  const rowIndex = useMemo(() => new Map(rows.map((row, i) => [row.node.id, i])), [rows])
  const current = active && rowIndex.has(active) ? active : rows[0]?.node.id ?? null

  useEffect(() => {
    if (!focused || !rowIndex.has(focused)) return
    const element = document.getElementById(`invite-member-${focused}`)
    element?.focus({ preventScroll: true })
    element?.scrollIntoView({ block: 'center', behavior: 'smooth' })
  }, [focused, rowIndex])

  const toggle = (id: string) => {
    if (focused) update({ member: null }, true)
    setExpanded(() => {
      const next = new Set(openSet)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }
  const focusRow = (id: string | undefined) => {
    if (!id) return
    setActive(id)
    document.getElementById(`invite-member-${id}`)?.focus()
  }
  // The WAI-ARIA tree pattern: arrows move, Right/Left open and close.
  const onKeyDown = (e: KeyboardEvent<HTMLUListElement>) => {
    if (!current) return
    const i = rowIndex.get(current) ?? 0
    const row = rows[i]
    const hasChildren = row.node.children.length > 0
    const handled = (() => {
      switch (e.key) {
        case 'ArrowDown': focusRow(rows[i + 1]?.node.id); return true
        case 'ArrowUp': focusRow(rows[i - 1]?.node.id); return true
        case 'Home': focusRow(rows[0]?.node.id); return true
        case 'End': focusRow(rows[rows.length - 1]?.node.id); return true
        case 'ArrowRight':
          if (hasChildren && !isOpen(row.node) && !searching) toggle(row.node.id)
          else if (hasChildren) focusRow(row.node.children[0].id)
          return true
        case 'ArrowLeft':
          if (hasChildren && isOpen(row.node) && !searching) toggle(row.node.id)
          else focusRow(row.parent ?? undefined)
          return true
        case 'Enter':
          if (row.node.kind === 'member' && (e.target as HTMLElement).getAttribute('role') === 'treeitem')
            document.querySelector<HTMLAnchorElement>(`#invite-member-${row.node.id} a`)?.click()
          return true
        default: return false
      }
    })()
    if (handled) e.preventDefault()
  }

  const setView = (next: 'mine' | 'all') => {
    setNoBranch(false)
    setExpanded(null)
    update({ view: next === 'all' ? 'all' : null, q: null, member: null })
  }
  const findMe = () => {
    if (!me) return
    setExpanded(null)
    update({ q: null, member: me })
  }

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-2 text-lg font-bold">Invite tree</h1>
      <p className="text-muted-foreground mb-3 text-sm">Who joined through whose invite. A former member stands in for an inviter no longer here, and members who hide their network appear without their name.</p>
      <Button variant="outline" size="sm" render={<Link to="/invites" />} className="mb-4">Your invites</Button>
      {!token ? <div className="space-y-2"><p className="text-sm">Sign in to view the invite tree.</p><Button onClick={() => beginLogin()}>Sign in</Button></div> : <>
        {error && <div role="alert" className="text-destructive mb-3 text-sm">{error} <Button size="sm" variant="outline" onClick={() => setReload(n => n + 1)}>Try again</Button></div>}
        {!tree && !error && <p role="status" className="text-muted-foreground text-sm">Loading…</p>}
        {tree && <>
          <p className="text-muted-foreground mb-3 text-sm">{tree.total} {tree.total === 1 ? 'member' : 'members'} in the instance</p>
          <div className="mb-3 flex flex-wrap gap-2">
            <Button size="sm" variant={effectiveView === 'mine' ? 'secondary' : 'outline'} aria-pressed={effectiveView === 'mine'} disabled={noBranch}
              onClick={() => setView('mine')}>My branch</Button>
            <Button size="sm" variant={effectiveView === 'all' ? 'secondary' : 'outline'} aria-pressed={effectiveView === 'all'} onClick={() => setView('all')}>Whole instance</Button>
            <Button size="sm" variant="outline" disabled={!me || !parents.has(me)} onClick={findMe}>Find me</Button>
            <Button size="sm" variant="ghost" disabled={searching} onClick={() => { setExpanded(new Set()); update({ member: null }) }}>Collapse all</Button>
          </div>
          <div className="mb-3 space-y-1">
            <Label htmlFor="tree-search">Search members</Label>
            <Input id="tree-search" value={query} onChange={e => update({ q: e.target.value || null, member: null }, true)} placeholder={effectiveView === 'mine' ? 'Search your branch' : 'Search the whole instance'} />
          </div>
          {tree.members.length === 0 ? <p className="text-muted-foreground text-sm">No members yet.</p>
            : roots.length === 0 ? <p role="status" className="text-muted-foreground text-sm">No matching members in this view.</p>
            : <ul role="tree" aria-label="Invite lineage" aria-busy={loading} className="space-y-1" onKeyDown={onKeyDown}>{roots.map(node => <TreeNode key={node.id} node={node} level={1}
              {...{ isOpen, toggle, counts, searching, canGrant, me, focused }} active={current} setActive={setActive} />)}</ul>}
        </>}
      </>}
    </div>
  )
}
