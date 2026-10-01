import { useCallback, useEffect, useState, type ReactNode } from 'react'
import { Link, useParams } from 'react-router-dom'
import { toast } from 'sonner'

import {
  getAccount,
  getReport,
  publicAccount,
  transitionReport,
  updateReport,
  type AccountActionType,
  type AdminAccount,
  type AdminReport,
  type ReportCategory,
  type ReportTransition,
} from '../../admin-api.ts'
import { getInstanceRules, type InstanceRule } from '../../api.ts'
import { getToken } from '../../auth.ts'
import { getMeId } from '../../me.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import {
  AccountStateBadges,
  AdminAccountLink,
  AdminStatus,
  ChoiceSelect,
  formatDate,
} from '@/components/admin/admin-common.tsx'
import { AccountActionDialog } from '@/components/admin/account-action-dialog.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Label } from '@/components/ui/label.tsx'
import { CATEGORY_LABELS } from './Reports.tsx'

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="space-y-1">
      <dt className="text-muted-foreground text-xs font-medium">{label}</dt>
      <dd className="text-sm">{children}</dd>
    </div>
  )
}

/**
 * The category and the rules it names, editable as Mastodon's report page lets
 * a moderator recategorise what a reporter chose. Rules belong to the
 * `violation` category only, so they are offered — and sent — with it alone.
 */
function CategoryEditor({
  report,
  rules,
  token,
  onSaved,
}: {
  report: AdminReport
  rules: InstanceRule[]
  token: string
  onSaved: (r: AdminReport) => void
}) {
  const [category, setCategory] = useState<ReportCategory>(report.category)
  const [ruleIds, setRuleIds] = useState<string[]>(report.rules.map((r) => r.id))
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    setCategory(report.category)
    setRuleIds(report.rules.map((r) => r.id))
  }, [report])

  const dirty =
    category !== report.category ||
    ruleIds.slice().sort().join() !== report.rules.map((r) => r.id).sort().join()

  const save = async () => {
    setSaving(true)
    try {
      const updated = await updateReport(token, report.id, {
        category,
        rule_ids: category === 'violation' ? ruleIds : [],
      })
      onSaved(updated)
      toast.success('Report updated.')
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setSaving(false)
    }
  }

  const items: Record<ReportCategory, string> =
    rules.length > 0 || report.category === 'violation'
      ? CATEGORY_LABELS
      : { spam: 'Spam', legal: 'Legal', other: 'Other' } as Record<ReportCategory, string>

  return (
    <div className="space-y-2">
      <ChoiceSelect
        label="Category"
        value={category}
        items={items}
        onChange={setCategory}
        className="w-full sm:w-64"
      />
      {category === 'violation' && rules.length > 0 && (
        <div className="space-y-1.5">
          {rules.map((rule) => (
            <Label key={rule.id} className="items-start text-sm font-normal leading-snug">
              <Checkbox
                checked={ruleIds.includes(rule.id)}
                onCheckedChange={(on) =>
                  setRuleIds((ids) =>
                    on ? [...ids, rule.id] : ids.filter((id) => id !== rule.id),
                  )
                }
              />
              <span>{rule.text}</span>
            </Label>
          ))}
        </div>
      )}
      {dirty && (
        <Button size="sm" disabled={saving} onClick={() => void save()}>
          {saving ? 'Saving…' : 'Save category'}
        </Button>
      )}
    </div>
  )
}

/**
 * One report: who filed it against whom, what they said and pointed at, who
 * is handling it, and what can be done — Mastodon's admin report page on the
 * report and account-action APIs.
 */
export default function ReportDetail() {
  const { id = '' } = useParams()
  const token = getToken()
  const [report, setReport] = useState<AdminReport | null>(null)
  const [target, setTarget] = useState<AdminAccount | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [rules, setRules] = useState<InstanceRule[]>([])
  const [busy, setBusy] = useState(false)
  const [action, setAction] = useState<AccountActionType | 'custom' | null>(null)

  const load = useCallback(() => {
    if (!token) return
    getReport(token, id)
      .then((r) => {
        setReport(r)
        setError(null)
        // The report's own copy is an admin account in Mastodon. Fetched on its
        // own as well, so the actions below know the account's current state
        // — local or remote, already limited or suspended — whatever the
        // report carried.
        return getAccount(token, publicAccount(r.target_account).id).then(setTarget)
      })
      .catch((e) => setError(String(e)))
  }, [token, id])

  useEffect(() => {
    load()
  }, [load])

  useEffect(() => {
    getInstanceRules().then(setRules).catch(() => {})
  }, [])

  const transition = async (t: ReportTransition, done: string) => {
    if (!token || busy) return
    setBusy(true)
    try {
      setReport(await transitionReport(token, id, t))
      toast.success(done)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const meId = getMeId()
  const assignedToMe =
    !!report?.assigned_account && publicAccount(report.assigned_account).id === meId

  return (
    <AdminLayout
      title={`Report #${id}`}
      permission="manage_reports"
      actions={
        report && (
          <Badge variant={report.action_taken ? 'outline' : 'secondary'}>
            {report.action_taken ? 'Resolved' : 'Unresolved'}
          </Badge>
        )
      }
    >
      <AdminError error={error} />
      {!report && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {report && token && (
        <div className="space-y-5">
          <section className="space-y-3 rounded-lg border p-3">
            <div className="flex flex-wrap items-center gap-2">
              <div className="min-w-0 flex-1">
                <AdminAccountLink account={report.target_account} />
              </div>
              {target && <AccountStateBadges account={target} />}
            </div>
            <div className="flex flex-wrap gap-2">
              <Button
                size="sm"
                variant="outline"
                disabled={busy || report.action_taken}
                onClick={() => void transition('resolve', 'Report resolved.')}
              >
                Mark as resolved
              </Button>
              {target && (
                <>
                  {target.domain === null && (
                    <Button size="sm" variant="outline" onClick={() => setAction('none')}>
                      Warn
                    </Button>
                  )}
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={target.silenced || target.suspended}
                    onClick={() => setAction('silence')}
                  >
                    Limit
                  </Button>
                  <Button
                    size="sm"
                    variant="destructive"
                    disabled={target.suspended}
                    onClick={() => setAction('suspend')}
                  >
                    Suspend
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setAction('custom')}>
                    Other action…
                  </Button>
                </>
              )}
            </div>
            <p className="text-muted-foreground text-xs">
              Limiting or suspending resolves every open report against this account;
              a warning resolves this one.{' '}
              <Link to={`/admin/accounts/${publicAccount(report.target_account).id}`}>
                Account details
              </Link>
            </p>
          </section>

          <dl className="grid gap-4 sm:grid-cols-2">
            <Field label="Reported by">
              <AdminAccountLink account={report.account} size="sm" />
            </Field>
            <Field label="Filed">{formatDate(report.created_at)}</Field>
            <Field label="Category">
              <CategoryEditor report={report} rules={rules} token={token} onSaved={setReport} />
            </Field>
            <Field label="Forwarded">
              {report.forwarded ? 'Yes, from a remote server' : 'No'}
            </Field>
            <Field label="Assigned moderator">
              <div className="flex flex-wrap items-center gap-2">
                {report.assigned_account ? (
                  <AdminAccountLink account={report.assigned_account} size="sm" />
                ) : (
                  <span className="text-muted-foreground">Nobody</span>
                )}
                {assignedToMe ? (
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={busy}
                    onClick={() => void transition('unassign', 'Unassigned.')}
                  >
                    Unassign
                  </Button>
                ) : (
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={busy}
                    onClick={() => void transition('assign_to_self', 'Assigned to you.')}
                  >
                    Assign to me
                  </Button>
                )}
              </div>
            </Field>
            {report.action_taken && (
              <Field label="Resolved">
                <div className="space-y-1">
                  <span className="block">{formatDate(report.action_taken_at)}</span>
                  {report.action_taken_by_account && (
                    <AdminAccountLink account={report.action_taken_by_account} size="sm" />
                  )}
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={busy}
                    onClick={() => void transition('reopen', 'Report reopened.')}
                  >
                    Reopen
                  </Button>
                </div>
              </Field>
            )}
          </dl>

          {report.rules.length > 0 && report.category === 'violation' && (
            <section className="space-y-1">
              <h2 className="text-sm font-semibold">Rules broken</h2>
              <ol className="list-decimal space-y-0.5 pl-5 text-sm">
                {report.rules.map((r) => (
                  <li key={r.id}>{r.text}</li>
                ))}
              </ol>
            </section>
          )}

          <section className="space-y-1">
            <h2 className="text-sm font-semibold">Comment</h2>
            <p className="text-sm whitespace-pre-wrap">
              {report.comment || (
                <span className="text-muted-foreground">The reporter left no comment.</span>
              )}
            </p>
          </section>

          <section className="space-y-2">
            <h2 className="text-sm font-semibold">
              Reported posts ({report.statuses.length})
            </h2>
            {report.statuses.length === 0 && (
              <p className="text-muted-foreground text-sm">No posts were attached.</p>
            )}
            {report.statuses.map((s) => (
              <AdminStatus key={s.id} status={s} />
            ))}
          </section>

          <p className="text-sm">
            <Link
              to={`/admin/reports?target_account_id=${publicAccount(report.target_account).id}`}
            >
              Other open reports about @{publicAccount(report.target_account).acct}
            </Link>
          </p>
        </div>
      )}
      {target && token && report && (
        <AccountActionDialog
          account={target}
          reportId={report.id}
          token={token}
          open={action !== null}
          initialType={action === 'custom' || action === null ? undefined : action}
          onOpenChange={(open) => !open && setAction(null)}
          onDone={load}
        />
      )}
    </AdminLayout>
  )
}
