import { useEffect, useState, type FormEvent } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { toast } from 'sonner'

import {
  can,
  createDomainAllow,
  deleteDomainAllow,
  listActionLogs,
  type ActionLog,
  type Measure,
} from '../../admin-api.ts'
import {
  createInstanceNote,
  deleteInstanceNote,
  getInstance,
  getInstanceMeasures,
  instanceDeliveryAction,
  purgeInstance,
  type AdminInstanceDetail,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { SuggestionCard } from './ModerationSubscriptions.tsx'
import { AdminError, AdminLayout, useRolePermissions } from '@/components/admin/admin-layout.tsx'
import { AdminAccountLink, ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { ActionLogEntry } from '@/components/admin/action-log-entry.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { policyText } from './Instances.tsx'
import { CardsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const MEASURES: { key: string; label: string }[] = [
  { key: 'instance_accounts', get label() { return t({ id: 'mastodon.admin.accounts.title', defaultMessage: 'Accounts' }) } },
  { key: 'instance_statuses', get label() { return t({ id: 'account.posts', defaultMessage: 'Posts' }) } },
  { key: 'instance_media_attachments', get label() { return t({ id: 'mastodon.admin.dashboard.media_storage', defaultMessage: 'Media storage' }) } },
  { key: 'instance_follows', get label() { return t({ id: 'eunha.admin.instance_detail.followers_here', defaultMessage: 'Followers here' }) } },
  { key: 'instance_followers', get label() { return t({ id: 'eunha.admin.instance_detail.following_from_here', defaultMessage: 'Following from here' }) } },
  { key: 'instance_reports', get label() { return t({ id: 'mastodon.admin.reports.title', defaultMessage: 'Reports' }) } },
]

const DAY = 24 * 60 * 60 * 1000

/**
 * One server: Mastodon's `Admin::InstancesController#show`, with its totals,
 * content policy, audit log, notes and availability.
 */
export default function InstanceDetail() {
  const { domain = '' } = useParams()
  const navigate = useNavigate()
  const token = getToken()
  const permissions = useRolePermissions()
  const [instance, setInstance] = useState<AdminInstanceDetail | null>(null)
  const [measures, setMeasures] = useState<Measure[]>([])
  const [logs, setLogs] = useState<ActionLog[]>([])
  const [error, setError] = useState<string | null>(null)
  const [note, setNote] = useState('')

  const load = () => {
    if (!token) return
    getInstance(token, domain)
      .then((i) => {
        setInstance(i)
        setError(null)
      })
      .catch((e) => setError(String(e)))
    void (async () => {
      for await (const page of listActionLogs(token, { target_domain: domain })) {
        setLogs(page.slice(0, 5))
        break
      }
    })().catch(() => setLogs([]))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token, domain])
  useEffect(() => {
    if (!token || permissions === null || !can(permissions, 'view_dashboard')) return
    const end = new Date()
    const start = new Date(end.getTime() - 6 * DAY)
    getInstanceMeasures(
      token,
      domain,
      MEASURES.map((m) => m.key),
      start.toISOString().slice(0, 10),
      end.toISOString().slice(0, 10),
    )
      .then(setMeasures)
      .catch(() => setMeasures([]))
  }, [token, domain, permissions])

  const act = async (run: () => Promise<AdminInstanceDetail | unknown>, done: string) => {
    try {
      const result = await run()
      if (result && typeof result === 'object' && 'domain' in result) {
        setInstance(result as AdminInstanceDetail)
      } else {
        load()
      }
      toast.success(done)
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  const addNote = async (e: FormEvent) => {
    e.preventDefault()
    await act(() => createInstanceNote(token ?? '', domain, note), t({ id: 'eunha.admin.instance_detail.note_added', defaultMessage: 'Note added.' }))
    setNote('')
  }

  return (
    <AdminLayout title={domain} permission="manage_federation">
      <AdminError error={error} />
      {instance === null && !error && <CardsSkeleton count={6} className="sm:grid-cols-3" />}
      {instance && (
        <div className="space-y-6">
          {measures.length > 0 && (
            <section>
              <p className="text-muted-foreground mb-2 text-xs">
                {t({ id: 'eunha.admin.instance_detail.totals_are_of_all_time_the', defaultMessage: 'Totals are of all time; the period is the last seven days.' })}
              </p>
              <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
                {MEASURES.map((m) => {
                  const measure = measures.find((x) => x.key === m.key)
                  return (
                    <Card key={m.key}>
                      <CardContent className="py-3">
                        <div className="text-lg font-bold tabular-nums">
                          {measure?.human_value ?? measure?.total ?? '–'}
                        </div>
                        <div className="text-muted-foreground text-xs">{m.label}</div>
                      </CardContent>
                    </Card>
                  )
                })}
              </div>
            </section>
          )}
          {!instance.persisted && (
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.admin.instance_detail.this_server_has_no_accounts_here', defaultMessage: 'This server has no accounts here, and is neither blocked nor allowed.' })}
            </p>
          )}

          {instance.moderation && (
            <section className="space-y-2">
              <h2 className="text-sm font-semibold">{t({ id: 'eunha.admin.instance_detail.advisories', defaultMessage: 'Advisories' })}</h2>
              <SuggestionCard target={instance.moderation} onChanged={load} />
            </section>
          )}

          <section className="space-y-2">
            <h2 className="text-sm font-semibold">{t({ id: 'mastodon.admin.instances.content_policies.title', defaultMessage: 'Content policies' })}</h2>
            {instance.domain_block ? (
              <div className="space-y-1 text-sm">
                <p>{policyText(instance.domain_block)}</p>
                {instance.domain_block.private_comment && (
                  <p className="text-muted-foreground">
                    {t({ id: 'eunha.admin.instance_detail.comment', defaultMessage: 'Comment: {text}' }, { text: instance.domain_block.private_comment })}
                  </p>
                )}
                {instance.domain_block.public_comment && (
                  <p className="text-muted-foreground">
                    {t({ id: 'eunha.admin.instance_detail.reason', defaultMessage: 'Reason: {text}' }, { text: instance.domain_block.public_comment })}
                  </p>
                )}
                {instance.moderation_subscription && (
                  <p className="text-muted-foreground">
                    {t({ id: 'eunha.admin.instance_detail.moderation_subscription', defaultMessage: 'Moderation subscription:' })}{' '}
                    <Link to={`/admin/moderation_subscriptions/${instance.moderation_subscription.id}`}>
                      {instance.moderation_subscription.name}
                    </Link>
                  </p>
                )}
                <Link to="/admin/domain_blocks" className="text-sm">
                  {t({ id: 'mastodon.admin.domain_blocks.edit', defaultMessage: 'Edit domain block' })}
                </Link>
              </div>
            ) : (
              <p className="text-muted-foreground text-sm">
                {t({ id: 'eunha.admin.canonical_email_blocks.not_blocked', defaultMessage: 'Not blocked.' })} <Link to="/admin/domain_blocks">{t({ id: 'eunha.admin.instance_detail.add_a_domain_block', defaultMessage: 'Add a domain block' })}</Link>
              </p>
            )}
            <div className="flex gap-2">
              {instance.domain_allow ? (
                <ConfirmButton
                  size="xs"
                  title={t({ id: 'eunha.admin.instance_detail.disallow_federation_with', defaultMessage: 'Disallow federation with {domain}?' }, { domain })}
                  description={t({ id: 'eunha.admin.instance_detail.in_limited_federation_mode_its_accounts', defaultMessage: 'In limited federation mode its accounts are suspended and then deleted.' })}
                  confirmLabel={t({ id: 'mastodon.admin.trends.disallow', defaultMessage: 'Disallow' })}
                  onConfirm={async () => {
                    await deleteDomainAllow(token ?? '', instance.domain_allow!.id)
                    toast.success(t({ id: 'eunha.admin.instance_detail.federation_disallowed', defaultMessage: 'Federation disallowed.' }))
                    load()
                  }}
                >
                  {t({ id: 'eunha.admin.instance_detail.disallow_federation', defaultMessage: 'Disallow federation' })}
                </ConfirmButton>
              ) : (
                <Button
                  size="xs"
                  variant="outline"
                  onClick={() =>
                    void act(() => createDomainAllow(token ?? '', domain), t({ id: 'eunha.admin.instance_detail.federation_allowed', defaultMessage: 'Federation allowed.' }))
                  }
                >
                  {t({ id: 'eunha.admin.instance_detail.allow_federation', defaultMessage: 'Allow federation' })}
                </Button>
              )}
            </div>
          </section>

          {logs.length > 0 && (
            <section className="space-y-2">
              <h2 className="text-sm font-semibold">{t({ id: 'mastodon.admin.action_logs.title', defaultMessage: 'Audit log' })}</h2>
              <div className="space-y-1">
                {logs.map((log) => (
                  <ActionLogEntry key={log.id} log={log} />
                ))}
              </div>
              <Link
                to={`/admin/action_logs?target_domain=${encodeURIComponent(domain)}`}
                className="text-sm"
              >
                {t({ id: 'follow_suggestions.view_all', defaultMessage: 'View all' })}
              </Link>
            </section>
          )}

          <section className="space-y-2">
            <h2 className="text-sm font-semibold">{t({ id: 'mastodon.admin.accounts.moderation_notes', defaultMessage: 'Moderation notes' })}</h2>
            <p className="text-muted-foreground text-xs">
              {t({ id: 'eunha.admin.instance_detail.moderation_notes_help_communicate_between_you', defaultMessage: 'Moderation notes help communicate between you and other moderators.' })}
            </p>
            {instance.moderation_notes.map((n) => (
              <div key={n.id} className="flex items-start gap-2 rounded-lg border p-2 text-sm">
                <div className="min-w-0 flex-1 space-y-1">
                  {n.account && <AdminAccountLink account={n.account} size="sm" />}
                  <p className="whitespace-pre-wrap">{n.content}</p>
                  <p className="text-muted-foreground text-xs">{formatDate(n.created_at)}</p>
                </div>
                <Button
                  size="xs"
                  variant="ghost"
                  onClick={() =>
                    void act(() => deleteInstanceNote(token ?? '', domain, n.id), t({ id: 'eunha.admin.instance_detail.note_deleted', defaultMessage: 'Note deleted.' }))
                  }
                >
                  {t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
                </Button>
              </div>
            ))}
            <form onSubmit={addNote} className="space-y-2">
              <Textarea
                aria-label={t({ id: 'eunha.admin.instance_detail.new_note', defaultMessage: 'New note' })}
                rows={3}
                maxLength={2000}
                value={note}
                placeholder={t({ id: 'eunha.admin.instance_detail.describe_what_actions_have_been_taken', defaultMessage: 'Describe what actions have been taken, or any other related updates…' })}
                onChange={(e) => setNote(e.target.value)}
              />
              <Button type="submit" size="sm" disabled={!note.trim()}>
                {t({ id: 'mastodon.admin.reports.notes.create', defaultMessage: 'Add note' })}
              </Button>
            </form>
          </section>

          {instance.persisted && (
            <section className="space-y-2">
              <h2 className="text-sm font-semibold">{t({ id: 'mastodon.admin.instances.availability.title', defaultMessage: 'Availability' })}</h2>
              <p className="text-muted-foreground text-xs">
                {t({ id: 'eunha.admin.instance_detail.if_deliveries_to_the_domain_fail', defaultMessage: 'If deliveries to the domain fail on 7 different days, no further deliveries are attempted until one from it is received.' })}
              </p>
              <ul className="flex gap-1" aria-label={t({ id: 'eunha.admin.instance_detail.deliveries_over_the_last_days', defaultMessage: 'Deliveries over the last days' })}>
                {instance.availability.map((d) => (
                  <li
                    key={d.date}
                    title={d.date}
                    className={`h-6 w-3 rounded-sm ${d.failing ? 'bg-destructive' : 'bg-muted'}`}
                  />
                ))}
              </ul>
              <div className="flex flex-wrap items-center gap-2 text-sm">
                {instance.unavailable ? (
                  <>
                    <span className="text-destructive">
                      {t({ id: 'mastodon.admin.instances.availability.failure_threshold_reached', defaultMessage: 'Failure threshold reached on {date}.' }, { date: formatDate(instance.unavailable_since) })}
                    </span>
                    <Button
                      size="xs"
                      variant="outline"
                      onClick={() =>
                        void act(
                          () => instanceDeliveryAction(token ?? '', domain, 'restart_delivery'),
                          t({ id: 'eunha.admin.instance_detail.delivery_restarted', defaultMessage: 'Delivery restarted.' }),
                        )
                      }
                    >
                      {t({ id: 'mastodon.admin.instances.delivery.restart', defaultMessage: 'Restart delivery' })}
                    </Button>
                  </>
                ) : (
                  <>
                    {instance.exhausted_deliveries_days.length === 0 ? (
                      <span className="text-muted-foreground">{t({ id: 'eunha.admin.instance_detail.no_failures_recorded', defaultMessage: 'No failures recorded.' })}</span>
                    ) : (
                      <>
                        <span className="text-destructive">
                          {t({ id: 'eunha.admin.instance_detail.failed_attempts', defaultMessage: 'Failed attempts on {count, plural, one {# different day} other {# different days}}.' }, { count: instance.exhausted_deliveries_days.length })}
                        </span>
                        <Button
                          size="xs"
                          variant="outline"
                          onClick={() =>
                            void act(
                              () =>
                                instanceDeliveryAction(token ?? '', domain, 'clear_delivery_errors'),
                              t({ id: 'eunha.admin.instance_detail.delivery_errors_cleared', defaultMessage: 'Delivery errors cleared.' }),
                            )
                          }
                        >
                          {t({ id: 'mastodon.admin.accounts.reset', defaultMessage: 'Reset' })}
                        </Button>
                      </>
                    )}
                    <Button
                      size="xs"
                      variant="outline"
                      onClick={() =>
                        void act(
                          () => instanceDeliveryAction(token ?? '', domain, 'stop_delivery'),
                          t({ id: 'eunha.admin.instance_detail.delivery_stopped', defaultMessage: 'Delivery stopped.' }),
                        )
                      }
                    >
                      {t({ id: 'mastodon.admin.instances.delivery.stop', defaultMessage: 'Stop delivery' })}
                    </Button>
                  </>
                )}
              </div>
              {instance.purgeable && (
                <div className="space-y-2 pt-2">
                  <p className="text-muted-foreground text-xs">
                    {t({ id: 'mastodon.admin.instances.purge_description_html', defaultMessage: 'If you believe this domain is offline for good, you can delete all account records and associated data from this domain from your storage. This may take a while.' })}
                  </p>
                  <ConfirmButton
                    title={t({ id: 'eunha.admin.instance_detail.purge', defaultMessage: 'Purge {domain}?' }, { domain })}
                    description={t({ id: 'eunha.admin.instance_detail.every_account_and_post_from_this', defaultMessage: 'Every account and post from this domain is deleted. This cannot be undone.' })}
                    confirmLabel={t({ id: 'mastodon.admin.instances.purge', defaultMessage: 'Purge' })}
                    onConfirm={async () => {
                      await purgeInstance(token ?? '', domain)
                      toast.success(t({ id: 'eunha.admin.instance_detail.data_from_is_being_purged', defaultMessage: 'Data from {domain} is being purged.' }, { domain }))
                      navigate('/admin/instances')
                    }}
                  >
                    {t({ id: 'mastodon.admin.instances.purge', defaultMessage: 'Purge' })}
                  </ConfirmButton>
                </div>
              )}
            </section>
          )}
        </div>
      )}
    </AdminLayout>
  )
}
