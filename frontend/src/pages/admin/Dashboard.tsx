import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'

import {
  can,
  getDimensions,
  getMeasures,
  getRetention,
  listAccounts,
  listReports,
  listTrendingTags,
  type AdminTag,
  type Cohort,
  type Dimension,
  type Measure,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { AdminError, AdminLayout, useRolePermissions } from '@/components/admin/admin-layout.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table.tsx'

// Mastodon's dashboard, in its order: the five counters, then the dimensions.
const MEASURES: { key: string; label: string; href?: string }[] = [
  { key: 'new_users', label: 'New users' },
  { key: 'active_users', label: 'Active users' },
  { key: 'interactions', label: 'Interactions' },
  { key: 'opened_reports', label: 'Reports opened', href: '/admin/reports' },
  { key: 'resolved_reports', label: 'Reports resolved', href: '/admin/reports?resolved=true' },
]

const DIMENSIONS: { key: string; label: string }[] = [
  { key: 'sources', label: 'Sign-up sources' },
  { key: 'languages', label: 'Top active languages' },
  { key: 'servers', label: 'Top active servers' },
  { key: 'software_versions', label: 'Software' },
  { key: 'space_usage', label: 'Space usage' },
]

const DAY = 24 * 60 * 60 * 1000

function isoDate(d: Date): string {
  return d.toISOString().slice(0, 10)
}

/**
 * A single series across the period: no axes, no legend — the tile's title
 * names it — and a hover target per day that says the date and the value.
 */
function Sparkline({ data, label }: { data: Measure['data']; label: string }) {
  if (data.length < 2) return null
  const values = data.map((d) => Number(d.value) || 0)
  const max = Math.max(1, ...values)
  const w = 100
  const h = 28
  const step = w / (values.length - 1)
  const points = values.map((v, i) => `${(i * step).toFixed(2)},${(h - 2 - (v / max) * (h - 4)).toFixed(2)}`)
  return (
    <svg
      viewBox={`0 0 ${w} ${h}`}
      preserveAspectRatio="none"
      className="text-primary h-8 w-full"
      role="img"
      aria-label={`${label} per day`}
    >
      <polyline
        points={points.join(' ')}
        fill="none"
        stroke="currentColor"
        strokeWidth={2}
        vectorEffect="non-scaling-stroke"
        strokeLinejoin="round"
        strokeLinecap="round"
      />
      {data.map((d, i) => (
        <rect
          key={d.date}
          x={Math.max(0, i * step - step / 2)}
          y={0}
          width={step}
          height={h}
          fill="transparent"
        >
          <title>
            {new Date(d.date).toLocaleDateString()}: {d.value}
          </title>
        </rect>
      ))}
    </svg>
  )
}

function MeasureTile({ measure, label, href }: { measure?: Measure; label: string; href?: string }) {
  const total = measure ? Number(measure.total) : null
  const previous = measure?.previous_total !== undefined ? Number(measure.previous_total) : null
  const change =
    total !== null && previous !== null && previous > 0
      ? Math.round(((total - previous) / previous) * 100)
      : null
  const inner = (
    <Card className="h-full py-3">
      <CardContent className="space-y-1 px-3">
        <div className="text-muted-foreground text-xs font-medium">{label}</div>
        <div className="flex items-baseline gap-2">
          <span className="text-2xl font-semibold tabular-nums">
            {measure ? (measure.human_value ?? measure.total) : '—'}
          </span>
          {change !== null && (
            <span className="text-muted-foreground text-xs tabular-nums">
              {change > 0 ? '+' : ''}
              {change}%
            </span>
          )}
        </div>
        {measure && <Sparkline data={measure.data} label={label} />}
      </CardContent>
    </Card>
  )
  return href ? (
    <Link to={href} className="no-underline">
      {inner}
    </Link>
  ) : (
    inner
  )
}

/** A dimension as a ranked list, each row's bar its share of the largest. */
function DimensionCard({ dimension, label }: { dimension?: Dimension; label: string }) {
  const rows = dimension?.data ?? []
  const max = Math.max(1, ...rows.map((r) => Number(r.value) || 0))
  return (
    <Card className="py-3">
      <CardContent className="space-y-2 px-3">
        <h2 className="text-sm font-semibold">{label}</h2>
        {rows.length === 0 && <p className="text-muted-foreground text-xs">No data</p>}
        <ul className="space-y-1.5">
          {rows.map((r) => (
            <li key={r.key} className="space-y-0.5" title={`${r.human_key}: ${r.human_value ?? r.value}`}>
              <div className="flex justify-between gap-2 text-xs">
                <span className="truncate">{r.human_key}</span>
                <span className="text-muted-foreground shrink-0 tabular-nums">
                  {r.human_value ?? r.value}
                </span>
              </div>
              <div className="bg-muted h-1 rounded-full">
                <div
                  className="bg-primary h-1 rounded-full"
                  style={{ width: `${((Number(r.value) || 0) / max) * 100}%` }}
                />
              </div>
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  )
}

/** Monthly cohorts: how many who signed up in a month were still active later. */
function RetentionTable({ cohorts }: { cohorts: Cohort[] }) {
  if (cohorts.length === 0) return null
  const width = Math.max(...cohorts.map((c) => c.data.length))
  return (
    <Card className="py-3">
      <CardContent className="space-y-2 px-3">
        <h2 className="text-sm font-semibold">User retention</h2>
        <Table className="text-xs tabular-nums">
          <TableHeader>
            <TableRow>
              <TableHead>Sign-up month</TableHead>
              <TableHead className="text-right">Users</TableHead>
              {Array.from({ length: width }, (_, i) => (
                <TableHead key={i} className="text-right">
                  {i}
                </TableHead>
              ))}
            </TableRow>
          </TableHeader>
          <TableBody>
            {cohorts.map((c) => (
              <TableRow key={c.period}>
                <TableCell>
                  {new Date(c.period).toLocaleDateString(undefined, {
                    year: 'numeric',
                    month: 'short',
                  })}
                </TableCell>
                <TableCell className="text-right">{c.data[0]?.value ?? 0}</TableCell>
                {Array.from({ length: width }, (_, i) => {
                  const d = c.data[i]
                  return (
                    <TableCell
                      key={i}
                      className="text-right"
                      title={d ? `${d.value} users (${Math.round(d.rate * 100)}%)` : undefined}
                      style={
                        d
                          ? {
                              backgroundColor: `color-mix(in oklab, var(--primary) ${Math.round(d.rate * 60)}%, transparent)`,
                            }
                          : undefined
                      }
                    >
                      {d ? `${Math.round(d.rate * 100)}%` : ''}
                    </TableCell>
                  )
                })}
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  )
}

/**
 * Counts the first page of a queue: exact below a page, "N+" at or above it.
 * Mastodon's dashboard reads these counts from the database; the admin API has
 * no count endpoint, so a page is what there is.
 */
async function firstPageCount(list: AsyncIterable<unknown[]>): Promise<string> {
  const it = list[Symbol.asyncIterator]()
  const { value } = await it.next()
  const n = value?.length ?? 0
  return n >= 40 ? `${n}+` : String(n)
}

/**
 * Mastodon's admin dashboard on `POST /api/v1/admin/measures`, `/dimensions`
 * and `/retention`, over the last 30 days, with what is waiting for a
 * moderator above it.
 */
export default function Dashboard() {
  const token = getToken()
  const permissions = useRolePermissions() ?? 0
  const [measures, setMeasures] = useState<Measure[]>([])
  const [dimensions, setDimensions] = useState<Dimension[]>([])
  const [cohorts, setCohorts] = useState<Cohort[]>([])
  const [pending, setPending] = useState<{ label: string; count: string; href: string }[]>([])
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!token) return
    const end = new Date()
    const start = new Date(end.getTime() - 29 * DAY)
    getMeasures(token, MEASURES.map((m) => m.key), isoDate(start), isoDate(end))
      .then(setMeasures)
      .catch((e) => setError(String(e)))
    getDimensions(token, DIMENSIONS.map((d) => d.key), isoDate(start), isoDate(end))
      .then(setDimensions)
      .catch(() => {})
    const retentionStart = new Date(end.getFullYear(), end.getMonth() - 5, 1)
    getRetention(token, isoDate(retentionStart), isoDate(end), 'month')
      .then(setCohorts)
      .catch(() => {})
  }, [token])

  useEffect(() => {
    if (!token) return
    const queues: Promise<{ label: string; count: string; href: string } | null>[] = [
      can(permissions, 'manage_reports')
        ? firstPageCount(listReports(token, { limit: 40 })).then((count) => ({
            label: 'Pending reports',
            count,
            href: '/admin/reports',
          }))
        : Promise.resolve(null),
      can(permissions, 'manage_users')
        ? firstPageCount(listAccounts(token, { status: 'pending', limit: 40 })).then((count) => ({
            label: 'Pending users',
            count,
            href: '/admin/accounts?status=pending',
          }))
        : Promise.resolve(null),
      can(permissions, 'manage_taxonomies')
        ? (async () => {
            const it = listTrendingTags(token)[Symbol.asyncIterator]()
            const { value } = await it.next()
            const n = ((value ?? []) as AdminTag[]).filter((t) => t.requires_review).length
            return { label: 'Hashtags to review', count: String(n), href: '/admin/trends/tags' }
          })()
        : Promise.resolve(null),
    ]
    Promise.all(queues.map((q) => q.catch(() => null))).then((rows) =>
      setPending(rows.filter((r): r is { label: string; count: string; href: string } => !!r)),
    )
  }, [token, permissions])

  const byKey = <T extends { key: string }>(list: T[], key: string) =>
    list.find((x) => x.key === key)

  return (
    <AdminLayout title="Dashboard" permission="view_dashboard">
      <AdminError error={error} />
      <div className="space-y-4">
        {pending.length > 0 && (
          <div className="grid gap-2 sm:grid-cols-3">
            {pending.map((p) => (
              <Link
                key={p.label}
                to={p.href}
                className="hover:bg-muted/40 flex items-center justify-between rounded-lg border px-3 py-2 no-underline"
              >
                <span className="text-sm">{p.label}</span>
                <span className="font-semibold tabular-nums">{p.count}</span>
              </Link>
            ))}
          </div>
        )}
        <p className="text-muted-foreground text-xs">Last 30 days, against the 30 before.</p>
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
          {MEASURES.map((m) => (
            <MeasureTile key={m.key} measure={byKey(measures, m.key)} label={m.label} href={m.href} />
          ))}
        </div>
        <div className="grid gap-2 sm:grid-cols-2">
          {DIMENSIONS.map((d) => (
            <DimensionCard key={d.key} dimension={byKey(dimensions, d.key)} label={d.label} />
          ))}
        </div>
        <RetentionTable cohorts={cohorts} />
      </div>
    </AdminLayout>
  )
}
