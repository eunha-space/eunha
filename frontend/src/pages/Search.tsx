import { useEffect, useState, type FormEvent } from 'react'
import { MastoHttpError } from 'masto'
import { toast } from 'sonner'
import { Link, useLocation, useSearchParams } from 'react-router-dom'
import { Check, CircleAlert, LoaderCircle } from 'lucide-react'

import type { mastodon } from '../masto.ts'
import { search } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { AccountRow } from '@/components/account-row.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'

type StepState = 'running' | 'done' | 'failed'

interface Step {
  key: string
  label: string
  state: StepState
  found?: number
  error?: unknown
}

interface Found {
  accounts: mastodon.v1.Account[]
  hashtags: mastodon.v1.Tag[]
  statuses: mastodon.v1.Status[]
}

const nothing: Found = { accounts: [], hashtags: [], statuses: [] }

// `@user@example.com` or `user@example.com`: a handle WebFinger can look up.
const HANDLE = /^@?[\w.-]+@([\w-]+(?:\.[\w-]+)+)$/

function host(url: string) {
  try {
    return new URL(url).host
  } catch {
    return url
  }
}

function uniqueById<T extends { id: string }>(items: T[]) {
  const seen = new Set<string>()
  return items.filter((item) => !seen.has(item.id) && !!seen.add(item.id))
}

/** The parts of a search, each its own request, so that what this server has
 *  shows at once and a slow post search or a remote fetch shows as a step
 *  still running rather than holding everything back. */
function planSearch(q: string, signedIn: boolean) {
  type Part = { key: string; label: string; run: (token?: string) => Promise<Found> }
  const pick = (r: mastodon.v2.Search): Found => ({
    accounts: r.accounts,
    hashtags: r.hashtags,
    statuses: r.statuses,
  })
  const parts: Part[] = []
  if (signedIn && /^https?:\/\//.test(q)) {
    // A URL is fetched rather than searched for.
    parts.push({
      key: 'remote',
      label: `Fetching from ${host(q)}`,
      run: (token) => search(q, token).then(pick),
    })
    return parts
  }
  parts.push({
    key: 'accounts',
    label: 'People on this server',
    run: (token) => search(q, token, { type: 'accounts', resolve: false }).then(pick),
  })
  const handle = signedIn ? HANDLE.exec(q) : null
  if (handle) {
    parts.push({
      key: 'remote',
      label: `Looking up ${q.startsWith('@') ? q : `@${q}`} on ${handle[1]}`,
      run: (token) => search(q, token, { type: 'accounts' }).then(pick),
    })
  }
  parts.push({
    key: 'hashtags',
    label: 'Hashtags',
    run: (token) => search(q, token, { type: 'hashtags', resolve: false }).then(pick),
  })
  if (signedIn) {
    parts.push({
      key: 'statuses',
      label: 'Posts',
      run: (token) => search(q, token, { type: 'statuses', resolve: false }).then(pick),
    })
  }
  return parts
}

function StepIcon({ state }: { state: StepState }) {
  if (state === 'running') return <LoaderCircle className="size-3.5 animate-spin" />
  if (state === 'failed') return <CircleAlert className="text-destructive size-3.5" />
  return <Check className="size-3.5" />
}

export default function Search() {
  const [params, setParams] = useSearchParams()
  const { pathname, search: queryString } = useLocation()
  const q = params.get('q') ?? ''
  const token = getToken()

  const [input, setInput] = useState(q)
  const [steps, setSteps] = useState<Step[]>([])
  const [found, setFound] = useState<Record<string, Found>>({})

  useEffect(() => setInput(q), [q])

  useEffect(() => {
    setFound({})
    const query = q.trim()
    if (!query) {
      setSteps([])
      return
    }
    let cancelled = false
    const parts = planSearch(query, !!token)
    setSteps(parts.map(({ key, label }) => ({ key, label, state: 'running' })))
    const update = (key: string, patch: Partial<Step>) =>
      setSteps((all) => all.map((step) => (step.key === key ? { ...step, ...patch } : step)))
    for (const part of parts) {
      part
        .run(token ?? undefined)
        .then((r) => {
          if (cancelled) return
          setFound((all) => ({ ...all, [part.key]: r }))
          update(part.key, {
            state: 'done',
            found: r.accounts.length + r.hashtags.length + r.statuses.length,
          })
        })
        .catch((error) => !cancelled && update(part.key, { state: 'failed', error }))
    }
    return () => {
      cancelled = true
    }
  }, [q, token])

  const submit = (e: FormEvent) => {
    e.preventDefault()
    setParams(input.trim() ? { q: input.trim() } : {})
  }

  const errors = steps.flatMap((step) => (step.error != null ? [step.error] : []))
  const needsSignIn =
    !!token && errors.some((e) => e instanceof MastoHttpError && e.statusCode === 401)
  // What a remote lookup found comes first: it is the exact match.
  const ordered = ['remote', 'accounts', 'hashtags', 'statuses'].map((k) => found[k] ?? nothing)
  const results: Found = {
    accounts: uniqueById(ordered.flatMap((r) => r.accounts)),
    hashtags: ordered.flatMap((r) => r.hashtags),
    statuses: uniqueById(ordered.flatMap((r) => r.statuses)),
  }
  const finished = steps.length > 0 && steps.every((step) => step.state !== 'running')
  const empty =
    finished &&
    results.accounts.length === 0 &&
    results.hashtags.length === 0 &&
    results.statuses.length === 0

  return (
    <div className="page-frame">
      <TopBar />
      <form onSubmit={submit} className="mb-4">
        <Input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="Search posts, people, and hashtags"
          autoFocus
        />
      </form>

      {needsSignIn ? (
        <div className="mb-4 space-y-2">
          <p className="text-muted-foreground text-sm">Sign in again to search for remote accounts and posts.</p>
          <Button
            size="sm"
            onClick={() => beginLogin(false, pathname + queryString).catch(() => toast.error('Could not start sign-in'))}
          >
            Sign in again
          </Button>
        </div>
      ) : null}
      {steps.length > 0 && (
        <ul aria-label="Search progress" className="text-muted-foreground mb-4 space-y-1 text-sm">
          {steps.map((step) => (
            <li key={step.key} className="flex items-center gap-2">
              <StepIcon state={step.state} />
              <span>
                {step.label}
                {step.state === 'running' && '…'}
                {step.state === 'done' &&
                  (step.found ? ` · ${step.found} found` : ' · nothing found')}
                {step.state === 'failed' &&
                  !(step.error instanceof MastoHttpError && step.error.statusCode === 401) &&
                  ` · ${step.error instanceof Error ? step.error.message : 'failed'}`}
              </span>
            </li>
          ))}
        </ul>
      )}

      <div className="space-y-4">
        {results.accounts.length > 0 && (
          <section>
            <h2 className="text-muted-foreground mb-2 text-sm font-semibold">People</h2>
            <div className="space-y-1">
              {results.accounts.map((a) => (
                <AccountRow key={a.id} account={a} />
              ))}
            </div>
          </section>
        )}

        {results.hashtags.length > 0 && (
          <section>
            <h2 className="text-muted-foreground mb-2 text-sm font-semibold">Hashtags</h2>
            <div className="space-y-1">
              {results.hashtags.map((t) => (
                <Link
                  key={t.name}
                  to={`/tags/${t.name}`}
                  className="hover:bg-muted/50 block rounded-lg p-2 font-medium no-underline"
                >
                  #{t.name}
                </Link>
              ))}
            </div>
          </section>
        )}

        {results.statuses.length > 0 && (
          <section>
            <h2 className="text-muted-foreground mb-2 text-sm font-semibold">Posts</h2>
            <TimelineStack>
              {results.statuses.map((s) => (
                <StatusCard
                  key={s.id}
                  status={s.reblog ?? s}
                  token={token ?? ''}
                  boostedBy={s.reblog ? s.account : undefined}
                />
              ))}
            </TimelineStack>
          </section>
        )}

        {empty && (
          <p className="text-muted-foreground text-sm">No results for “{q}”.</p>
        )}
      </div>
    </div>
  )
}
