import { useEffect, useState, type FormEvent } from 'react'
import { MastoHttpError } from 'masto'
import { toast } from 'sonner'
import { Link, useLocation, useSearchParams } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { search } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { AccountRow } from '@/components/account-row.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'

export default function Search() {
  const [params, setParams] = useSearchParams()
  const { pathname, search: queryString } = useLocation()
  const q = params.get('q') ?? ''
  const token = getToken()

  const [input, setInput] = useState(q)
  const [results, setResults] = useState<mastodon.v2.Search | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<unknown>(null)

  useEffect(() => setInput(q), [q])

  useEffect(() => {
    setResults(null)
    setError(null)
    if (!q.trim()) {
      setLoading(false)
      return
    }
    let cancelled = false
    setLoading(true)
    search(q, token ?? undefined)
      .then((r) => !cancelled && setResults(r))
      .catch((e) => !cancelled && setError(e))
      .finally(() => !cancelled && setLoading(false))
    return () => {
      cancelled = true
    }
  }, [q, token])

  const submit = (e: FormEvent) => {
    e.preventDefault()
    setParams(input.trim() ? { q: input.trim() } : {})
  }

  const needsSignIn = !!token && error instanceof MastoHttpError && error.statusCode === 401
  const empty =
    results != null &&
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
      ) : error != null && <p className="text-destructive text-sm">{String(error)}</p>}
      {loading && <p className="text-muted-foreground text-sm">Searching…</p>}

      {results && (
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
      )}
    </div>
  )
}
