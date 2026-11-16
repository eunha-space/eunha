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
import { AccountListSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

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
      label: t({ id: 'eunha.search.fetching_from', defaultMessage: 'Fetching from {host}' }, { host: host(q) }),
      run: (token) => search(q, token).then(pick),
    })
    return parts
  }
  parts.push({
    key: 'accounts',
    label: t({ id: 'eunha.search.people_on_this_server', defaultMessage: 'People on this server' }),
    run: (token) => search(q, token, { type: 'accounts', resolve: false }).then(pick),
  })
  const handle = signedIn ? HANDLE.exec(q) : null
  if (handle) {
    parts.push({
      key: 'remote',
      label: t({ id: 'eunha.search.looking_up', defaultMessage: 'Looking up {handle} on {host}' }, { handle: q.startsWith('@') ? q : `@${q}`, host: handle[1] }),
      run: (token) => search(q, token, { type: 'accounts' }).then(pick),
    })
  }
  parts.push({
    key: 'hashtags',
    label: t({ id: 'search_results.hashtags', defaultMessage: 'Hashtags' }),
    run: (token) => search(q, token, { type: 'hashtags', resolve: false }).then(pick),
  })
  if (signedIn) {
    parts.push({
      key: 'statuses',
      label: t({ id: 'account.posts', defaultMessage: 'Posts' }),
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
          placeholder={t({ id: 'eunha.search.search_posts_people_and_hashtags', defaultMessage: 'Search posts, people, and hashtags' })}
          autoFocus
        />
      </form>

      {needsSignIn ? (
        <div className="mb-4 space-y-2">
          <p className="text-muted-foreground text-sm">{t({ id: 'eunha.search.sign_in_again_to_search_for', defaultMessage: 'Sign in again to search for remote accounts and posts.' })}</p>
          <Button
            size="sm"
            onClick={() => beginLogin(false, pathname + queryString).catch(() => toast.error(t({ id: 'eunha.search.could_not_start_sign_in', defaultMessage: 'Could not start sign-in' })))}
          >
            {t({ id: 'eunha.search.sign_in_again', defaultMessage: 'Sign in again' })}
          </Button>
        </div>
      ) : null}
      {steps.length > 0 && (
        <ul aria-label={t({ id: 'eunha.search.search_progress', defaultMessage: 'Search progress' })} className="text-muted-foreground mb-4 space-y-1 text-sm">
          {steps.map((step) => (
            <li key={step.key} className="flex items-center gap-2">
              <StepIcon state={step.state} />
              <span>
                {step.label}
                {step.state === 'running' && '…'}
                {step.state === 'done' &&
                  (step.found ? t({ id: 'eunha.search.found', defaultMessage: ' · {found} found' }, { found: step.found }) : ' · nothing found')}
                {step.state === 'failed' &&
                  !(step.error instanceof MastoHttpError && step.error.statusCode === 401) &&
                  ` · ${step.error instanceof Error ? step.error.message : 'failed'}`}
              </span>
            </li>
          ))}
        </ul>
      )}

      <div className="space-y-4">
        {steps.length > 0 &&
          !finished &&
          results.accounts.length === 0 &&
          results.hashtags.length === 0 &&
          results.statuses.length === 0 && <AccountListSkeleton count={3} />}
        {results.accounts.length > 0 && (
          <section>
            <h2 className="text-muted-foreground mb-2 text-sm font-semibold">{t({ id: 'explore.suggested_follows', defaultMessage: 'People' })}</h2>
            <div className="space-y-1">
              {results.accounts.map((a) => (
                <AccountRow key={a.id} account={a} />
              ))}
            </div>
          </section>
        )}

        {results.hashtags.length > 0 && (
          <section>
            <h2 className="text-muted-foreground mb-2 text-sm font-semibold">{t({ id: 'search_results.hashtags', defaultMessage: 'Hashtags' })}</h2>
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
            <h2 className="text-muted-foreground mb-2 text-sm font-semibold">{t({ id: 'account.posts', defaultMessage: 'Posts' })}</h2>
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
          <p className="text-muted-foreground text-sm">{t({ id: 'eunha.search.no_results_for', defaultMessage: 'No results for “{q}”.' }, { q })}</p>
        )}
      </div>
    </div>
  )
}
