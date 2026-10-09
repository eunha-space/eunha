import { useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { ChevronDown, ChevronRight } from 'lucide-react'

import { getInviteTree, type InviteNode, type InviteTree } from '../eunha-api.ts'
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
  inviter_unavailable: 'Inviter is unavailable in this view',
  lineage_unavailable: 'Earlier lineage is unavailable',
}

function pathTo(nodes: InviteNode[], id: string): InviteNode[] | null {
  for (const node of nodes) {
    if (node.id === id) return [node]
    const path = pathTo(node.children, id)
    if (path) return [node, ...path]
  }
  return null
}

function searchTree(nodes: InviteNode[], query: string): InviteNode[] {
  return nodes.flatMap(node => {
    const children = searchTree(node.children, query)
    const matches = `${node.acct} ${node.display_name}`.toLowerCase().includes(query)
    return matches || children.length ? [{ ...node, children }] : []
  })
}

function TreeNode({ node, expanded, toggle, counts, searching, canGrant, me, focused, depth = 0 }: {
  node: InviteNode
  expanded: Set<string>
  toggle: (id: string) => void
  counts: Map<string, number>
  searching: boolean
  canGrant: boolean
  me: string | undefined
  focused: string | null
  depth?: number
}) {
  const name = node.display_name || node.username
  const open = searching || expanded.has(node.id)
  const count = counts.get(node.id) ?? 0
  const avatar = useAnimatedImage(node.avatar, node.avatar_static)
  return (
    <li>
      <div id={`invite-member-${node.id}`} tabIndex={-1}
        className={`rounded-lg p-2 ${focused === node.id ? 'bg-muted ring-primary ring-2' : ''}`}>
        <div className="flex min-w-0 items-center gap-2">
          {node.children.length > 0 ? <Button size="icon-sm" variant="ghost"
            aria-label={`${open ? 'Collapse' : 'Expand'} @${node.acct}`}
            aria-expanded={open} aria-controls={`invite-children-${node.id}`}
            disabled={searching} onClick={() => toggle(node.id)}>
            {open ? <ChevronDown /> : <ChevronRight />}
          </Button> : <span className="size-7 shrink-0" />}
          <Link to={`/@${node.acct}`} className="flex min-w-0 flex-1 items-center gap-2 no-underline">
            <Avatar className="size-8 shrink-0" {...avatar.hover}>
              <AvatarImage src={avatar.src} alt="" />
              <AvatarFallback>{name.slice(0, 1).toUpperCase()}</AvatarFallback>
            </Avatar>
            <div className="min-w-0">
              <div className="truncate font-medium">{name}{node.id === me && <span className="text-muted-foreground text-xs"> (you)</span>}</div>
              <div className="text-muted-foreground truncate text-sm">@{node.acct}</div>
            </div>
          </Link>
        </div>
        <div className="text-muted-foreground ml-9 mt-1 flex flex-wrap gap-x-3 gap-y-1 text-xs">
          <span>{count} direct {count === 1 ? 'invitee' : 'invitees'}</span>
          <time dateTime={node.invited_at}>Joined {new Date(node.invited_at).toLocaleDateString()}</time>
          {node.root_reason && <span>{ROOT_REASONS[node.root_reason]}</span>}
          {canGrant && <Link to={`/admin/invites?grant_to=${encodeURIComponent(node.id)}`} className="underline"
            aria-label={`Hand out invites to @${node.acct}`}>Hand out invites</Link>}
        </div>
      </div>
      {node.children.length > 0 && open && (
        <ul id={`invite-children-${node.id}`} className={depth < 4 ? "border-muted ml-3 border-l pl-1 sm:ml-5 sm:pl-2" : "border-muted border-l"}>
          {node.children.map(child => <TreeNode key={child.id} node={child} depth={depth + 1}
            {...{ expanded, toggle, counts, searching, canGrant, me, focused }} />)}
        </ul>
      )}
    </li>
  )
}

export default function InviteTree() {
  const token = getToken()
  const me = getSavedAccounts().find(account => account.token === token)?.account.id
  const [tree, setTree] = useState<InviteTree | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [canGrant, setCanGrant] = useState(false)
  const [view, setView] = useState<'mine' | 'all'>('mine')
  const [query, setQuery] = useState('')
  const [expanded, setExpanded] = useState<Set<string>>(new Set())
  const [focused, setFocused] = useState<string | null>(null)
  const [reload, setReload] = useState(0)

  useEffect(() => {
    if (!token) return
    let active = true
    setError(null)
    getInviteTree(token).then(result => {
      if (!active) return
      setTree(result)
      const path = me ? pathTo(result.roots, me) : null
      setExpanded(new Set(path?.map(node => node.id) ?? []))
      if (!path) setView('all')
    }).catch(() => { if (active) setError('Could not load the invite tree. Please try again.') })
    getInvitePermissions(token).then(perms => { if (active) setCanGrant(perms.canGrant) }).catch(() => {})
    return () => { active = false }
  }, [token, me, reload])

  const myPath = useMemo(() => tree && me ? pathTo(tree.roots, me) : null, [tree, me])
  const counts = useMemo(() => {
    const result = new Map<string, number>()
    const walk = (nodes: InviteNode[]) => nodes.forEach(node => { result.set(node.id, node.children.length); walk(node.children) })
    if (tree) walk(tree.roots)
    return result
  }, [tree])
  const searching = query.trim().length > 0
  const roots = useMemo(() => {
    if (!tree) return []
    let nodes = tree.roots
    if (view === 'mine' && myPath) {
      let branch = myPath[myPath.length - 1]
      for (let i = myPath.length - 2; i >= 0; i--) branch = { ...myPath[i], children: [branch] }
      nodes = [branch]
    }
    return searching ? searchTree(nodes, query.trim().toLowerCase()) : nodes
  }, [tree, view, myPath, searching, query])

  useEffect(() => {
    if (!focused) return
    const element = document.getElementById(`invite-member-${focused}`)
    element?.focus({ preventScroll: true })
    element?.scrollIntoView({ block: 'center', behavior: 'smooth' })
  }, [focused, roots, expanded])

  const toggle = (id: string) => {
    setFocused(null)
    setExpanded(current => {
      const next = new Set(current)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }
  const findMe = () => {
    setQuery('')
    setView('all')
    setExpanded(new Set(myPath?.map(node => node.id) ?? []))
    setFocused(me ?? null)
  }

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-2 text-lg font-bold">Invite tree</h1>
      <p className="text-muted-foreground mb-3 text-sm">Who joined through whose invite. Branches show direct invitees; roots may have no recorded inviter or an inviter unavailable in this view.</p>
      <Button variant="outline" size="sm" render={<Link to="/invites" />} className="mb-4">Your invites</Button>
      {!token ? <div className="space-y-2"><p className="text-sm">Sign in to view the invite tree.</p><Button onClick={() => beginLogin()}>Sign in</Button></div> : <>
        {error && <div role="alert" className="text-destructive mb-3 text-sm">{error} <Button size="sm" variant="outline" onClick={() => setReload(n => n + 1)}>Try again</Button></div>}
        {!tree && !error && <p role="status" className="text-muted-foreground text-sm">Loading…</p>}
        {tree && <>
          <p className="text-muted-foreground mb-3 text-sm">{tree.total} {tree.total === 1 ? 'member' : 'members'} in the instance</p>
          <div className="mb-3 flex flex-wrap gap-2">
            <Button size="sm" variant={view === 'mine' ? 'secondary' : 'outline'} aria-pressed={view === 'mine'} disabled={!myPath}
              onClick={() => { setView('mine'); setQuery(''); setFocused(null); setExpanded(new Set(myPath?.map(node => node.id) ?? [])) }}>My branch</Button>
            <Button size="sm" variant={view === 'all' ? 'secondary' : 'outline'} aria-pressed={view === 'all'} onClick={() => { setView('all'); setFocused(null) }}>Whole instance</Button>
            <Button size="sm" variant="outline" disabled={!myPath} onClick={findMe}>Find me</Button>
            <Button size="sm" variant="ghost" disabled={searching} onClick={() => { setExpanded(new Set()); setFocused(null) }}>Collapse all</Button>
          </div>
          <div className="mb-3 space-y-1">
            <Label htmlFor="tree-search">Search members</Label>
            <Input id="tree-search" value={query} onChange={e => { setQuery(e.target.value); setFocused(null) }} placeholder={view === 'mine' ? 'Search your branch' : 'Search the whole instance'} />
          </div>
          {tree.roots.length === 0 ? <p className="text-muted-foreground text-sm">No members yet.</p>
            : roots.length === 0 ? <p role="status" className="text-muted-foreground text-sm">No matching members in this view.</p>
            : <ul aria-label="Invite lineage" className="space-y-1">{roots.map(node => <TreeNode key={node.id} node={node}
              {...{ expanded, toggle, counts, searching, canGrant, me, focused }} />)}</ul>}
        </>}
      </>}
    </div>
  )
}
