import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'

import {
  can,
  getDimensions,
  getMeasures,
  getRetention,
  type Cohort,
  type Dimension,
  type Measure,
  type Permission,
} from '../../admin-api.ts'
import { getDashboard, type SystemCheck } from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { AdminError, AdminLayout, useHosted, useRolePermissions } from '@/components/admin/admin-layout.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import { Skeleton } from '@/components/ui/skeleton.tsx'
import { LinesSkeleton } from '@/components/skeletons.tsx'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table.tsx'
import { currentLocale, t } from '@/i18n.tsx'

// Mastodon's dashboard, in its order: the five counters, then the dimensions.
const MEASURES: { key: string; label: string; href?: string }[] = [
  { key: 'new_users', get label() { return t({ id: 'admin.dashboard.retention.cohort_size', defaultMessage: 'New users' }) } },
  { key: 'active_users', get label() { return t({ id: 'server_banner.active_users_title', defaultMessage: 'Active users' }) } },
  { key: 'interactions', get label() { return t({ id: 'eunha.admin.dashboard.interactions', defaultMessage: 'Interactions' }) } },
  { key: 'opened_reports', get label() { return t({ id: 'eunha.admin.dashboard.reports_opened', defaultMessage: 'Reports opened' }) }, href: '/admin/reports' },
  { key: 'resolved_reports', get label() { return t({ id: 'eunha.admin.dashboard.reports_resolved', defaultMessage: 'Reports resolved' }) }, href: '/admin/reports?resolved=true' },
]

/** `server` marks what a hosted instance's administrators are not shown. */
const DIMENSIONS: { key: string; label: string; server?: true }[] = [
  { key: 'sources', get label() { return t({ id: 'mastodon.admin.dashboard.sources', defaultMessage: 'Sign-up sources' }) } },
  { key: 'languages', get label() { return t({ id: 'mastodon.admin.dashboard.top_languages', defaultMessage: 'Top active languages' }) } },
  { key: 'servers', get label() { return t({ id: 'mastodon.admin.dashboard.top_servers', defaultMessage: 'Top active servers' }) } },
  { key: 'software_versions', get label() { return t({ id: 'mastodon.admin.dashboard.software', defaultMessage: 'Software' }) }, server: true },
  { key: 'space_usage', get label() { return t({ id: 'mastodon.admin.dashboard.space', defaultMessage: 'Space usage' }) }, server: true },
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
      aria-label={t({ id: 'eunha.admin.dashboard.per_day', defaultMessage: '{label} per day' }, { label })}
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
            {new Date(d.date).toLocaleDateString(currentLocale())}: {d.value}
          </title>
        </rect>
      ))}
    </svg>
  )
}

function MeasureTile({
  measure,
  label,
  href,
  loading,
}: {
  measure?: Measure
  label: string
  href?: string
  loading: boolean
}) {
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
          {loading ? (
            <Skeleton className="h-8 w-16" />
          ) : (
            <span className="text-2xl font-semibold tabular-nums">
              {measure ? (measure.human_value ?? measure.total) : '—'}
            </span>
          )}
          {change !== null && (
            <span className="text-muted-foreground text-xs tabular-nums">
              {change > 0 ? '+' : ''}
              {change}%
            </span>
          )}
        </div>
        {loading ? <Skeleton className="h-8 w-full" /> : measure && <Sparkline data={measure.data} label={label} />}
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
function DimensionCard({
  dimension,
  label,
  loading,
}: {
  dimension?: Dimension
  label: string
  loading: boolean
}) {
  const rows = dimension?.data ?? []
  const max = Math.max(1, ...rows.map((r) => Number(r.value) || 0))
  return (
    <Card className="py-3">
      <CardContent className="space-y-2 px-3">
        <h2 className="text-sm font-semibold">{label}</h2>
        {loading && <LinesSkeleton count={4} />}
        {!loading && rows.length === 0 && <p className="text-muted-foreground text-xs">{t({ id: 'eunha.admin.dashboard.no_data', defaultMessage: 'No data' })}</p>}
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
        <h2 className="text-sm font-semibold">{t({ id: 'eunha.admin.dashboard.user_retention', defaultMessage: 'User retention' })}</h2>
        <Table className="text-xs tabular-nums">
          <TableHeader>
            <TableRow>
              <TableHead>{t({ id: 'admin.dashboard.retention.cohort', defaultMessage: 'Sign-up month' })}</TableHead>
              <TableHead className="text-right">{t({ id: 'eunha.admin.dashboard.users', defaultMessage: 'Users' })}</TableHead>
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
                  {new Date(c.period).toLocaleDateString(currentLocale(), {
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
 * Mastodon's admin dashboard on `POST /api/v1/admin/measures`, `/dimensions`
 * and `/retention`, over the last 30 days, with what is waiting for a
 * moderator above it.
 */
export default function Dashboard() {
  const token = getToken()
  const permissions = useRolePermissions() ?? 0
  const hosted = useHosted()
  const [measures, setMeasures] = useState<Measure[]>([])
  const [dimensions, setDimensions] = useState<Dimension[]>([])
  const [cohorts, setCohorts] = useState<Cohort[]>([])
  const [pending, setPending] = useState<{ label: string; count: string; href: string }[]>([])
  const [checks, setChecks] = useState<SystemCheck[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loadingMeasures, setLoadingMeasures] = useState(true)
  const [loadingDimensions, setLoadingDimensions] = useState(true)

  useEffect(() => {
    if (!token) return
    const end = new Date()
    const start = new Date(end.getTime() - 29 * DAY)
    getMeasures(token, MEASURES.map((m) => m.key), isoDate(start), isoDate(end))
      .then(setMeasures)
      .catch((e) => setError(String(e)))
      .finally(() => setLoadingMeasures(false))
    getDimensions(token, DIMENSIONS.map((d) => d.key), isoDate(start), isoDate(end))
      .then(setDimensions)
      .catch(() => {})
      .finally(() => setLoadingDimensions(false))
    const retentionStart = new Date(end.getFullYear(), end.getMonth() - 5, 1)
    getRetention(token, isoDate(retentionStart), isoDate(end), 'month')
      .then(setCohorts)
      .catch(() => {})
  }, [token])

  // `Admin::DashboardController#index`: what waits for a moderator, each
  // shown to a role that may act on it, and the system checks.
  useEffect(() => {
    if (!token) return
    getDashboard(token)
      .then((d) => {
        const rows: { label: string; count: string; href: string; permission: Permission }[] = [
          {
            label: t({ id: 'eunha.admin.dashboard.pending_reports', defaultMessage: 'Pending reports' }),
            count: String(d.pending_reports_count),
            href: '/admin/reports',
            permission: 'manage_reports',
          },
          {
            label: t({ id: 'eunha.admin.dashboard.pending_users', defaultMessage: 'Pending users' }),
            count: String(d.pending_users_count),
            href: '/admin/accounts?status=pending',
            permission: 'manage_users',
          },
          {
            label: t({ id: 'eunha.admin.dashboard.pending_hashtags', defaultMessage: 'Pending hashtags' }),
            count: String(d.pending_tags_count),
            href: '/admin/trends/tags',
            permission: 'manage_taxonomies',
          },
          {
            label: t({ id: 'eunha.admin.dashboard.pending_appeals', defaultMessage: 'Pending appeals' }),
            count: String(d.pending_appeals_count),
            href: '/admin/disputes/appeals',
            permission: 'manage_appeals',
          },
        ]
        setPending(rows.filter((r) => can(permissions, r.permission)))
        setChecks(d.system_checks)
      })
      .catch(() => {})
  }, [token, permissions])

  const byKey = <T extends { key: string }>(list: T[], key: string) =>
    list.find((x) => x.key === key)

  return (
    <AdminLayout title={t({ id: 'mastodon.admin.dashboard.title', defaultMessage: 'Dashboard' })} permission="view_dashboard">
      <AdminError error={error} />
      <div className="space-y-4">
        {checks.length > 0 && (
          <ul className="space-y-2">
            {checks.map((c) => (
              <li
                key={c.key}
                className={`flex flex-wrap items-center gap-2 rounded-lg border px-3 py-2 text-sm ${
                  c.critical ? 'border-destructive text-destructive' : ''
                }`}
              >
                <span className="min-w-0 flex-1">{c.message}</span>
                {c.action &&
                  (c.action.startsWith('/') ? (
                    <Link to={c.action}>{c.action_label}</Link>
                  ) : (
                    <a href={c.action} target="_blank" rel="noreferrer">
                      {c.action_label}
                    </a>
                  ))}
              </li>
            ))}
          </ul>
        )}
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
        <p className="text-muted-foreground text-xs">{t({ id: 'eunha.admin.dashboard.last_30_days_against_the_30', defaultMessage: 'Last 30 days, against the 30 before.' })}</p>
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
          {MEASURES.map((m) => (
            <MeasureTile
              key={m.key}
              measure={byKey(measures, m.key)}
              label={m.label}
              href={m.href}
              loading={loadingMeasures}
            />
          ))}
        </div>
        <div className="grid gap-2 sm:grid-cols-2">
          {DIMENSIONS.filter((d) => !(d.server && hosted)).map((d) => (
            <DimensionCard
              key={d.key}
              dimension={byKey(dimensions, d.key)}
              label={d.label}
              loading={loadingDimensions}
            />
          ))}
        </div>
        <RetentionTable cohorts={cohorts} />
      </div>
    </AdminLayout>
  )
}
