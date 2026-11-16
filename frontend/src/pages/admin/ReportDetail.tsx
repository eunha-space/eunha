import { useCallback, useEffect, useState, type ReactNode } from 'react'
import { Link, useParams } from 'react-router-dom'
import { toast } from 'sonner'

import {
  createReportNote,
  deleteReportNote,
  getAccount,
  getReport,
  getReportHistory,
  listReportNotes,
  reportModerationAction,
  publicAccount,
  transitionReport,
  updateReport,
  type AccountActionType,
  type ActionLog,
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
  ConfirmButton,
  formatDate,
} from '@/components/admin/admin-common.tsx'
import { AccountActionDialog } from '@/components/admin/account-action-dialog.tsx'
import { ActionLogEntry } from '@/components/admin/action-log-entry.tsx'
import { ModerationNotes } from '@/components/admin/moderation-notes.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Label } from '@/components/ui/label.tsx'
import { CATEGORY_LABELS } from './Reports.tsx'
import { DetailSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

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
      toast.success(t({ id: 'eunha.admin.report_detail.report_updated', defaultMessage: 'Report updated.' }))
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setSaving(false)
    }
  }

  const items: Record<ReportCategory, string> =
    rules.length > 0 || report.category === 'violation'
      ? CATEGORY_LABELS
      : { spam: t({ id: 'report.categories.spam', defaultMessage: 'Spam' }), legal: t({ id: 'report.categories.legal', defaultMessage: 'Legal' }), other: t({ id: 'report.categories.other', defaultMessage: 'Other' }) } as Record<ReportCategory, string>

  return (
    <div className="space-y-2">
      <ChoiceSelect
        label={t({ id: 'mastodon.admin.reports.category', defaultMessage: 'Category' })}
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
          {saving ? t({ id: 'eunha.common.saving', defaultMessage: 'Saving…' }) : t({ id: 'eunha.admin.report_detail.save_category', defaultMessage: 'Save category' })}
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
  const [history, setHistory] = useState<ActionLog[]>([])

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
    getReportHistory(token, id)
      .then(setHistory)
      .catch(() => setHistory([]))
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
      title={t({ id: 'mastodon.admin.reports.report', defaultMessage: 'Report #{id}' }, { id })}
      permission="manage_reports"
      actions={
        report && (
          <Badge variant={report.action_taken ? 'outline' : 'secondary'}>
            {report.action_taken ? t({ id: 'mastodon.admin.reports.resolved', defaultMessage: 'Resolved' }) : t({ id: 'mastodon.admin.reports.unresolved', defaultMessage: 'Unresolved' })}
          </Badge>
        )
      }
    >
      <AdminError error={error} />
      {!report && !error && <DetailSkeleton />}
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
                onClick={() => void transition('resolve', t({ id: 'eunha.admin.report_detail.report_resolved', defaultMessage: 'Report resolved.' }))}
              >
                {t({ id: 'mastodon.admin.reports.mark_as_resolved', defaultMessage: 'Mark as resolved' })}
              </Button>
              {report.statuses.length > 0 && (
                <>
                  <ConfirmButton
                    title={t({ id: 'eunha.admin.report_detail.mark_the_reported_posts_as_sensitive', defaultMessage: 'Mark the reported posts as sensitive?' })}
                    description={t({ id: 'eunha.admin.report_detail.their_media_is_hidden_behind_a', defaultMessage: 'Their media is hidden behind a warning, the report is resolved, and the account gets a strike.' })}
                    confirmLabel={t({ id: 'collections.mark_as_sensitive', defaultMessage: 'Mark as sensitive' })}
                    destructive={false}
                    disabled={report.action_taken}
                    onConfirm={async () => {
                      setReport(await reportModerationAction(token, report.id, 'mark_as_sensitive'))
                      toast.success(t({ id: 'eunha.admin.report_detail.posts_marked_as_sensitive', defaultMessage: 'Posts marked as sensitive.' }))
                      load()
                    }}
                  >
                    {t({ id: 'collections.mark_as_sensitive', defaultMessage: 'Mark as sensitive' })}
                  </ConfirmButton>
                  <ConfirmButton
                    title={t({ id: 'eunha.admin.report_detail.delete_the_reported_posts', defaultMessage: 'Delete the reported posts?' })}
                    description={t({ id: 'eunha.admin.report_detail.the_posts_are_removed_the_report', defaultMessage: 'The posts are removed, the report is resolved, and the account gets a strike. This cannot be undone.' })}
                    confirmLabel={t({ id: 'eunha.admin.report_detail.delete_posts', defaultMessage: 'Delete posts' })}
                    disabled={report.action_taken}
                    onConfirm={async () => {
                      setReport(await reportModerationAction(token, report.id, 'delete'))
                      toast.success(t({ id: 'eunha.admin.report_detail.posts_deleted', defaultMessage: 'Posts deleted.' }))
                      load()
                    }}
                  >
                    {t({ id: 'eunha.admin.report_detail.delete_posts', defaultMessage: 'Delete posts' })}
                  </ConfirmButton>
                </>
              )}
              {target && (
                <>
                  {target.domain === null && (
                    <Button size="sm" variant="outline" onClick={() => setAction('none')}>
                      {t({ id: 'mastodon.admin.accounts.warn', defaultMessage: 'Warn' })}
                    </Button>
                  )}
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={target.silenced || target.suspended}
                    onClick={() => setAction('silence')}
                  >
                    {t({ id: 'mastodon.admin.accounts.silence', defaultMessage: 'Limit' })}
                  </Button>
                  <Button
                    size="sm"
                    variant="destructive"
                    disabled={target.suspended}
                    onClick={() => setAction('suspend')}
                  >
                    {t({ id: 'mastodon.admin.accounts.perform_full_suspension', defaultMessage: 'Suspend' })}
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setAction('custom')}>
                    {t({ id: 'eunha.admin.report_detail.other_action', defaultMessage: 'Other action…' })}
                  </Button>
                </>
              )}
            </div>
            <p className="text-muted-foreground text-xs">
              {t({ id: 'eunha.admin.report_detail.resolution_hint', defaultMessage: 'Limiting or suspending resolves every open report against this account; a warning resolves this one.' })}{' '}
              <Link to={`/admin/accounts/${publicAccount(report.target_account).id}`}>
                {t({ id: 'eunha.admin.report_detail.account_details', defaultMessage: 'Account details' })}
              </Link>
            </p>
          </section>

          <dl className="grid gap-4 sm:grid-cols-2">
            <Field label={t({ id: 'mastodon.admin.reports.reported_by', defaultMessage: 'Reported by' })}>
              <AdminAccountLink account={report.account} size="sm" />
            </Field>
            <Field label={t({ id: 'eunha.admin.report_detail.filed', defaultMessage: 'Filed' })}>{formatDate(report.created_at)}</Field>
            <Field label={t({ id: 'mastodon.admin.reports.category', defaultMessage: 'Category' })}>
              <CategoryEditor report={report} rules={rules} token={token} onSaved={setReport} />
            </Field>
            <Field label={t({ id: 'mastodon.admin.reports.forwarded', defaultMessage: 'Forwarded' })}>
              {report.forwarded ? t({ id: 'eunha.admin.report_detail.yes_from_a_remote_server', defaultMessage: 'Yes, from a remote server' }) : t({ id: 'mastodon.simple_form.no', defaultMessage: 'No' })}
            </Field>
            <Field label={t({ id: 'mastodon.admin.reports.assigned', defaultMessage: 'Assigned moderator' })}>
              <div className="flex flex-wrap items-center gap-2">
                {report.assigned_account ? (
                  <AdminAccountLink account={report.assigned_account} size="sm" />
                ) : (
                  <span className="text-muted-foreground">{t({ id: 'eunha.admin.report_detail.nobody', defaultMessage: 'Nobody' })}</span>
                )}
                {assignedToMe ? (
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={busy}
                    onClick={() => void transition('unassign', t({ id: 'eunha.common.unassigned', defaultMessage: 'Unassigned.' }))}
                  >
                    {t({ id: 'mastodon.admin.reports.unassign', defaultMessage: 'Unassign' })}
                  </Button>
                ) : (
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={busy}
                    onClick={() => void transition('assign_to_self', t({ id: 'eunha.admin.report_detail.assigned_to_you', defaultMessage: 'Assigned to you.' }))}
                  >
                    {t({ id: 'mastodon.admin.reports.assign_to_self', defaultMessage: 'Assign to me' })}
                  </Button>
                )}
              </div>
            </Field>
            {report.action_taken && (
              <Field label={t({ id: 'mastodon.admin.reports.resolved', defaultMessage: 'Resolved' })}>
                <div className="space-y-1">
                  <span className="block">{formatDate(report.action_taken_at)}</span>
                  {report.action_taken_by_account && (
                    <AdminAccountLink account={report.action_taken_by_account} size="sm" />
                  )}
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={busy}
                    onClick={() => void transition('reopen', t({ id: 'eunha.admin.report_detail.report_reopened', defaultMessage: 'Report reopened.' }))}
                  >
                    {t({ id: 'eunha.admin.report_detail.reopen', defaultMessage: 'Reopen' })}
                  </Button>
                </div>
              </Field>
            )}
          </dl>

          {report.rules.length > 0 && report.category === 'violation' && (
            <section className="space-y-1">
              <h2 className="text-sm font-semibold">{t({ id: 'eunha.admin.report_detail.rules_broken', defaultMessage: 'Rules broken' })}</h2>
              <ol className="list-decimal space-y-0.5 pl-5 text-sm">
                {report.rules.map((r) => (
                  <li key={r.id}>{r.text}</li>
                ))}
              </ol>
            </section>
          )}

          <section className="space-y-1">
            <h2 className="text-sm font-semibold">{t({ id: 'mastodon.simple_form.labels.ip_block.comment', defaultMessage: 'Comment' })}</h2>
            <p className="text-sm whitespace-pre-wrap">
              {report.comment || (
                <span className="text-muted-foreground">{t({ id: 'eunha.admin.report_detail.the_reporter_left_no_comment', defaultMessage: 'The reporter left no comment.' })}</span>
              )}
            </p>
          </section>

          <section className="space-y-2">
            <div className="flex flex-wrap items-center gap-2">
              <h2 className="min-w-0 flex-1 text-sm font-semibold">
                {t({ id: 'eunha.admin.report_detail.reported_posts_count', defaultMessage: 'Reported posts ({count})' }, { count: report.statuses.length })}
              </h2>
              <Link
                className="text-xs"
                to={`/admin/accounts/${publicAccount(report.target_account).id}/statuses?report_id=${report.id}`}
              >
                {t({ id: 'eunha.admin.report_detail.add_posts', defaultMessage: 'Add posts' })}
              </Link>
            </div>
            {report.statuses.length === 0 && (
              <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.report_detail.no_posts_were_attached', defaultMessage: 'No posts were attached.' })}</p>
            )}
            {report.statuses.map((s) => (
              <AdminStatus key={s.id} status={s} />
            ))}
          </section>

          <ModerationNotes
            key={report.id}
            load={() => listReportNotes(token, report.id)}
            create={(content, flags) =>
              createReportNote(token, { report_id: report.id, content, ...flags })
            }
            remove={(noteId) => deleteReportNote(token, noteId)}
            extraSubmits={[
              report.action_taken
                ? { label: t({ id: 'eunha.admin.report_detail.add_note_and_reopen', defaultMessage: 'Add note and reopen' }), flags: { create_and_unresolve: true } }
                : { label: t({ id: 'eunha.admin.report_detail.add_note_and_resolve', defaultMessage: 'Add note and resolve' }), flags: { create_and_resolve: true } },
            ]}
            onCreated={(flags) => {
              if (Object.keys(flags).length > 0) load()
            }}
          />

          {history.length > 0 && (
            <section className="space-y-2">
              <h2 className="text-sm font-semibold">{t({ id: 'eunha.admin.report_detail.moderation_history', defaultMessage: 'Moderation history' })}</h2>
              <div className="divide-y rounded-lg border">
                {history.map((log) => (
                  <ActionLogEntry key={log.id} log={log} />
                ))}
              </div>
            </section>
          )}

          <p className="text-sm">
            <Link
              to={`/admin/reports?target_account_id=${publicAccount(report.target_account).id}`}
            >
              {t({ id: 'eunha.admin.report_detail.other_open_reports', defaultMessage: 'Other open reports about @{acct}' }, { acct: publicAccount(report.target_account).acct })}
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
