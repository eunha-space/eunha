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
import { AdminError, AdminLayout, useRolePermissions } from '@/components/admin/admin-layout.tsx'
import { AdminAccountLink, ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { ActionLogEntry } from '@/components/admin/action-log-entry.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { policyText } from './Instances.tsx'

const MEASURES: { key: string; label: string }[] = [
  { key: 'instance_accounts', label: 'Accounts' },
  { key: 'instance_statuses', label: 'Posts' },
  { key: 'instance_media_attachments', label: 'Media storage' },
  { key: 'instance_follows', label: 'Followers here' },
  { key: 'instance_followers', label: 'Following from here' },
  { key: 'instance_reports', label: 'Reports' },
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
    await act(() => createInstanceNote(token ?? '', domain, note), 'Note added.')
    setNote('')
  }

  return (
    <AdminLayout title={domain} permission="manage_federation">
      <AdminError error={error} />
      {instance === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {instance && (
        <div className="space-y-6">
          {measures.length > 0 && (
            <section>
              <p className="text-muted-foreground mb-2 text-xs">
                Totals are of all time; the period is the last seven days.
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
              This server has no accounts here, and is neither blocked nor allowed.
            </p>
          )}

          <section className="space-y-2">
            <h2 className="text-sm font-semibold">Content policies</h2>
            {instance.domain_block ? (
              <div className="space-y-1 text-sm">
                <p>{policyText(instance.domain_block)}</p>
                {instance.domain_block.private_comment && (
                  <p className="text-muted-foreground">
                    Comment: {instance.domain_block.private_comment}
                  </p>
                )}
                {instance.domain_block.public_comment && (
                  <p className="text-muted-foreground">
                    Reason: {instance.domain_block.public_comment}
                  </p>
                )}
                <Link to="/admin/domain_blocks" className="text-sm">
                  Edit domain block
                </Link>
              </div>
            ) : (
              <p className="text-muted-foreground text-sm">
                Not blocked. <Link to="/admin/domain_blocks">Add a domain block</Link>
              </p>
            )}
            <div className="flex gap-2">
              {instance.domain_allow ? (
                <ConfirmButton
                  size="xs"
                  title={`Disallow federation with ${domain}?`}
                  description="In limited federation mode its accounts are suspended and then deleted."
                  confirmLabel="Disallow"
                  onConfirm={async () => {
                    await deleteDomainAllow(token ?? '', instance.domain_allow!.id)
                    toast.success('Federation disallowed.')
                    load()
                  }}
                >
                  Disallow federation
                </ConfirmButton>
              ) : (
                <Button
                  size="xs"
                  variant="outline"
                  onClick={() =>
                    void act(() => createDomainAllow(token ?? '', domain), 'Federation allowed.')
                  }
                >
                  Allow federation
                </Button>
              )}
            </div>
          </section>

          {logs.length > 0 && (
            <section className="space-y-2">
              <h2 className="text-sm font-semibold">Audit log</h2>
              <div className="space-y-1">
                {logs.map((log) => (
                  <ActionLogEntry key={log.id} log={log} />
                ))}
              </div>
              <Link
                to={`/admin/action_logs?target_domain=${encodeURIComponent(domain)}`}
                className="text-sm"
              >
                View all
              </Link>
            </section>
          )}

          <section className="space-y-2">
            <h2 className="text-sm font-semibold">Moderation notes</h2>
            <p className="text-muted-foreground text-xs">
              Moderation notes help communicate between you and other moderators.
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
                    void act(() => deleteInstanceNote(token ?? '', domain, n.id), 'Note deleted.')
                  }
                >
                  Delete
                </Button>
              </div>
            ))}
            <form onSubmit={addNote} className="space-y-2">
              <Textarea
                aria-label="New note"
                rows={3}
                maxLength={2000}
                value={note}
                placeholder="Describe what actions have been taken, or any other related updates…"
                onChange={(e) => setNote(e.target.value)}
              />
              <Button type="submit" size="sm" disabled={!note.trim()}>
                Add note
              </Button>
            </form>
          </section>

          {instance.persisted && (
            <section className="space-y-2">
              <h2 className="text-sm font-semibold">Availability</h2>
              <p className="text-muted-foreground text-xs">
                If deliveries to the domain fail on 7 different days, no further deliveries
                are attempted until one from it is received.
              </p>
              <ul className="flex gap-1" aria-label="Deliveries over the last days">
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
                      Failure threshold reached on {formatDate(instance.unavailable_since)}.
                    </span>
                    <Button
                      size="xs"
                      variant="outline"
                      onClick={() =>
                        void act(
                          () => instanceDeliveryAction(token ?? '', domain, 'restart_delivery'),
                          'Delivery restarted.',
                        )
                      }
                    >
                      Restart delivery
                    </Button>
                  </>
                ) : (
                  <>
                    {instance.exhausted_deliveries_days.length === 0 ? (
                      <span className="text-muted-foreground">No failures recorded.</span>
                    ) : (
                      <>
                        <span className="text-destructive">
                          Failed attempts on {instance.exhausted_deliveries_days.length} different
                          days.
                        </span>
                        <Button
                          size="xs"
                          variant="outline"
                          onClick={() =>
                            void act(
                              () =>
                                instanceDeliveryAction(token ?? '', domain, 'clear_delivery_errors'),
                              'Delivery errors cleared.',
                            )
                          }
                        >
                          Reset
                        </Button>
                      </>
                    )}
                    <Button
                      size="xs"
                      variant="outline"
                      onClick={() =>
                        void act(
                          () => instanceDeliveryAction(token ?? '', domain, 'stop_delivery'),
                          'Delivery stopped.',
                        )
                      }
                    >
                      Stop delivery
                    </Button>
                  </>
                )}
              </div>
              {instance.purgeable && (
                <div className="space-y-2 pt-2">
                  <p className="text-muted-foreground text-xs">
                    If you believe this domain is offline for good, you can delete all account
                    records and associated data from this domain from your storage. This may
                    take a while.
                  </p>
                  <ConfirmButton
                    title={`Purge ${domain}?`}
                    description="Every account and post from this domain is deleted. This cannot be undone."
                    confirmLabel="Purge"
                    onConfirm={async () => {
                      await purgeInstance(token ?? '', domain)
                      toast.success(`Data from ${domain} is being purged.`)
                      navigate('/admin/instances')
                    }}
                  >
                    Purge
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
