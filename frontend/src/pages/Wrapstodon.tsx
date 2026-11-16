import { useEffect, useState } from 'react'
import { Link, useLocation } from 'react-router-dom'

import { getToken } from '../auth.ts'
import type { mastodon } from '../masto.ts'
import { AccountRow } from '@/components/account-row.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card.tsx'
import { DetailSkeleton } from '@/components/skeletons.tsx'
import { currentLocale, t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

// Mastodon's shared Wrapstodon page (`WrapstodonController#show`), at its
// `/@:account_username/wrapstodon/:year/:share_key`. The server puts the report
// in the page as `wrapstodon/show` does, in `#wrapstodon-data`: the
// `REST::AnnualReportsSerializer` payload, snake_case as it comes off the wire,
// plus the local `domain` and, for a signed-in viewer, `me`.

interface AnnualReport {
  year: number
  account_id: string
  share_url: string | null
  schema_version: number
  data: {
    archetype?: string
    top_statuses?: Record<string, string | null>
    time_series?: { month: number; statuses: number; followers: number }[]
    top_hashtags?: { name: string; count: number }[]
  } | null
}

interface SharedData {
  annual_reports: AnnualReport[]
  accounts: unknown[]
  statuses: unknown[]
  domain: string
  me?: string
}

type Loaded =
  | { kind: 'loading' }
  | { kind: 'found'; data: SharedData }
  // In limited federation mode the page carries the report only to a
  // signed-in viewer.
  | { kind: 'sign-in' }
  | { kind: 'missing' }
  | { kind: 'unavailable' }

// `annual_report.summary.archetype.*`.
const ARCHETYPES: Record<string, { name: string; description: (name: string, self: boolean) => string }> = {
  booster: {
    get name() { return t({ id: 'annual_report.summary.archetype.booster.name', defaultMessage: 'The Archer' }) },
    description: (name, self) =>
      self
        ? t({ id: 'annual_report.summary.archetype.booster.desc_self', defaultMessage: 'You stayed on the hunt for posts to boost, amplifying other creators with perfect aim.' })
        : t({ id: 'annual_report.summary.archetype.booster.desc_public', defaultMessage: '{name} stayed on the hunt for posts to boost, amplifying other creators with perfect aim.' }, { name }),
  },
  lurker: {
    get name() { return t({ id: 'annual_report.summary.archetype.lurker.name', defaultMessage: 'The Stoic' }) },
    description: (name, self) =>
      self
        ? t({ id: 'annual_report.summary.archetype.lurker.desc_self', defaultMessage: 'We know you were out there, somewhere, enjoying Mastodon in your own quiet way.' })
        : t({ id: 'annual_report.summary.archetype.lurker.desc_public', defaultMessage: 'We know {name} was out there, somewhere, enjoying Mastodon in their own quiet way.' }, { name }),
  },
  oracle: {
    get name() { return t({ id: 'annual_report.summary.archetype.oracle.name', defaultMessage: 'The Oracle' }) },
    description: (name, self) =>
      self
        ? t({ id: 'annual_report.summary.archetype.oracle.desc_self', defaultMessage: 'You created new posts more than replies, keeping Mastodon fresh and future-facing.' })
        : t({ id: 'annual_report.summary.archetype.oracle.desc_public', defaultMessage: '{name} created new posts more than replies, keeping Mastodon fresh and future-facing.' }, { name }),
  },
  pollster: {
    get name() { return t({ id: 'annual_report.summary.archetype.pollster.name', defaultMessage: 'The Wonderer' }) },
    description: (name, self) =>
      self
        ? t({ id: 'annual_report.summary.archetype.pollster.desc_self', defaultMessage: 'You created more polls than other post types, cultivating curiosity on Mastodon.' })
        : t({ id: 'annual_report.summary.archetype.pollster.desc_public', defaultMessage: '{name} created more polls than other post types, cultivating curiosity on Mastodon.' }, { name }),
  },
  replier: {
    get name() { return t({ id: 'annual_report.summary.archetype.replier.name', defaultMessage: 'The Butterfly' }) },
    description: (name, self) =>
      self
        ? t({ id: 'annual_report.summary.archetype.replier.desc_self', defaultMessage: 'You frequently replied to other people’s posts, pollinating Mastodon with new discussions.' })
        : t({ id: 'annual_report.summary.archetype.replier.desc_public', defaultMessage: '{name} frequently replied to other people’s posts, pollinating Mastodon with new discussions.' }, { name }),
  },
}

// masto.js hands entities back in camelCase; the cards here are masto's.
function camelize(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(camelize)
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value).map(([key, v]) => [
        key.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase()),
        camelize(v),
      ]),
    )
  }
  return value
}

function readData(doc: Document): SharedData | null {
  const node = doc.getElementById('wrapstodon-data')
  if (!node?.textContent) return null
  try {
    return JSON.parse(node.textContent) as SharedData
  } catch {
    return null
  }
}

function sharedPath(data: SharedData | null): string | null {
  const url = data?.annual_reports[0]?.share_url
  if (!url) return null
  try {
    return decodeURI(new URL(url).pathname)
  } catch {
    return null
  }
}

async function load(path: string): Promise<Loaded> {
  // What the server put in this page, when the page is this report's: a
  // visit that came through the app's own links loaded another page.
  const embedded = readData(document)
  if (embedded && sharedPath(embedded) === decodeURI(path)) {
    return { kind: 'found', data: embedded }
  }
  const token = getToken()
  const response = await fetch(path, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  })
  if (response.status === 404) return { kind: 'missing' }
  if (!response.ok) return { kind: 'unavailable' }
  const html = await response.text()
  const data = readData(new DOMParser().parseFromString(html, 'text/html'))
  if (data) return { kind: 'found', data }
  return token ? { kind: 'unavailable' } : { kind: 'sign-in' }
}

function Notice({ children }: { children: React.ReactNode }) {
  return (
    <main className="page-frame">
      <p className="text-muted-foreground text-sm">{children}</p>
    </main>
  )
}

export default function Wrapstodon() {
  const { pathname } = useLocation()
  const [loaded, setLoaded] = useState<Loaded>({ kind: 'loading' })

  useEffect(() => {
    let current = true
    setLoaded({ kind: 'loading' })
    load(pathname)
      .then((result) => current && setLoaded(result))
      .catch(() => current && setLoaded({ kind: 'unavailable' }))
    return () => {
      current = false
    }
  }, [pathname])

  switch (loaded.kind) {
    case 'loading':
      return (
        <main className="page-frame">
          <DetailSkeleton />
        </main>
      )
    case 'missing':
      return <Notice>{t({ id: 'eunha.wrapstodon.this_wrapstodon_could_not_be_found', defaultMessage: 'This Wrapstodon could not be found.' })}</Notice>
    case 'sign-in':
      return <Notice>{t({ id: 'eunha.wrapstodon.sign_in_to_see_this_wrapstodon', defaultMessage: 'Sign in to see this Wrapstodon.' })}</Notice>
    case 'unavailable':
      return <Notice>{t({ id: 'eunha.wrapstodon.this_wrapstodon_is_not_available', defaultMessage: 'This Wrapstodon is not available.' })}</Notice>
    case 'found':
      return <SharedReport data={loaded.data} />
  }
}

function SharedReport({ data }: { data: SharedData }) {
  const token = getToken() ?? ''
  const report = data.annual_reports[0]
  const accounts = camelize(data.accounts) as mastodon.v1.Account[]
  const statuses = camelize(data.statuses) as mastodon.v1.Status[]
  const account = accounts.find((a) => a.id === report?.account_id)
  if (!report || !account) return <Notice>{t({ id: 'eunha.wrapstodon.this_wrapstodon_is_not_available', defaultMessage: 'This Wrapstodon is not available.' })}</Notice>

  const name = account.displayName || account.username
  const self = data.me === account.id
  const archetype = report.data?.archetype ? ARCHETYPES[report.data.archetype] : undefined
  const topStatusId = Object.values(report.data?.top_statuses ?? {}).find(Boolean)
  const topStatus = statuses.find((s) => s.id === topStatusId)
  const year = report.data?.time_series?.find((m) => m.month === 12)
  const hashtag = report.data?.top_hashtags?.[0]

  return (
    <main className="page-frame space-y-4">
      <header className="space-y-3">
        <h1 className="text-2xl font-bold">{t({ id: 'eunha.wrapstodon.title', defaultMessage: 'Wrapstodon {year}' }, { year: report.year })}</h1>
        <AccountRow account={account} />
      </header>

      {archetype && (
        <Card>
          <CardHeader>
            <p className="text-muted-foreground text-sm">
              {self ? t({ id: 'annual_report.summary.archetype.title_self', defaultMessage: 'Your archetype' }) : t({ id: 'annual_report.summary.archetype.title_public', defaultMessage: '{name}\'\'s archetype' }, { name })}
            </p>
            <CardTitle className="text-xl">{archetype.name}</CardTitle>
          </CardHeader>
          <CardContent>
            <p>{archetype.description(name, self)}</p>
          </CardContent>
        </Card>
      )}

      {topStatus && (
        <section className="space-y-2">
          <h2 className="text-muted-foreground text-sm font-semibold">{t({ id: 'annual_report.summary.highlighted_post.title', defaultMessage: 'Most popular post' })}</h2>
          <StatusCard status={topStatus} token={token} />
        </section>
      )}

      <div className="grid grid-cols-2 gap-4">
        {year && (
          <>
            <Card>
              <CardContent>
                <p className="text-2xl font-bold">{year.statuses.toLocaleString(currentLocale())}</p>
                <p className="text-muted-foreground text-sm">{t({ id: 'annual_report.summary.new_posts.new_posts', defaultMessage: 'new posts' })}</p>
              </CardContent>
            </Card>
            <Card>
              <CardContent>
                <p className="text-2xl font-bold">{year.followers.toLocaleString(currentLocale())}</p>
                <p className="text-muted-foreground text-sm">
                  {year.followers === 1 ? 'new follower' : 'new followers'}
                </p>
              </CardContent>
            </Card>
          </>
        )}
        {hashtag && (
          <Card className="col-span-2">
            <CardContent>
              <Link to={`/tags/${encodeURIComponent(hashtag.name)}`} className="text-xl font-bold">
                #{hashtag.name}
              </Link>
              <p className="text-muted-foreground text-sm">
                {t({ id: 'annual_report.summary.most_used_hashtag.most_used_hashtag', defaultMessage: 'most used hashtag' })} ·{' '}
                {self
                  ? t({ id: 'annual_report.summary.most_used_hashtag.used_count', defaultMessage: 'You included this hashtag in {count, plural, one {one post} other {# posts}}.' }, { count: hashtag.count })
                  : t({ id: 'annual_report.summary.most_used_hashtag.used_count_public', defaultMessage: '{name} included this hashtag in {count, plural, one {one post} other {# posts}}.' }, { name, count: hashtag.count })}
              </p>
            </CardContent>
          </Card>
        )}
      </div>

      <footer className="text-muted-foreground space-y-1 border-t pt-4 text-sm">
        <p>
          <FormattedMessage
            id='annual_report.shared_page.footer_server_info'
            defaultMessage='{username} uses {domain}, one of many communities powered by Mastodon.'
            values={{ username: name, domain: <strong>{data.domain}</strong> }}
          />
        </p>
        <Link to="/about" className="underline">
          {t({ id: 'footer.about_server', defaultMessage: 'About {domain}' }, { domain: data.domain })}
        </Link>
      </footer>
    </main>
  )
}
