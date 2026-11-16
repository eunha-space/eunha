import { useCallback, useEffect, useState, type FormEvent } from 'react'
import { Link, useParams } from 'react-router-dom'
import { toast } from 'sonner'

import { appealStrike, can, decideAppeal, getStrike, type Strike } from '../admin-api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { useRolePermissions } from '@/components/admin/admin-layout.tsx'
import { AdminStatus, formatDate } from '@/components/admin/admin-common.tsx'
import { STRIKE_TITLE_ACTIONS, STRIKE_TITLES } from '@/components/admin/strike-card.tsx'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { DetailSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

/** `Appeal::TEXT_LENGTH_LIMIT`. */
const TEXT_LENGTH_LIMIT = 2000

function Detail({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="space-y-0.5">
      <dt className="text-muted-foreground text-xs font-medium">{label}</dt>
      <dd className="text-sm">{children}</dd>
    </div>
  )
}

/**
 * One strike: Mastodon's `disputes/strikes/show`. The account it was against
 * reads it and, within twenty days, appeals it once; staff who handle appeals
 * read any strike and decide the appeal from here.
 */
export default function StrikeDetail() {
  const { id = '' } = useParams()
  const token = getToken()
  const permissions = useRolePermissions() ?? 0
  const [strike, setStrike] = useState<Strike | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [text, setText] = useState('')
  const [busy, setBusy] = useState(false)

  const load = useCallback(() => {
    if (!token) return
    getStrike(token, id)
      .then((s) => {
        setStrike(s)
        setError(null)
      })
      .catch((e) => setError(errorMessage(e)))
  }, [token, id])

  useEffect(() => {
    load()
  }, [load])

  const appeal = async (e: FormEvent) => {
    e.preventDefault()
    if (!token || busy) return
    setBusy(true)
    try {
      setStrike(await appealStrike(token, id, text))
      setText('')
      toast.success(t({ id: 'mastodon.disputes.strikes.appealed_msg', defaultMessage: 'Your appeal has been submitted. If it is approved, you will be notified.' }))
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setBusy(false)
    }
  }

  const decide = async (decision: 'approve' | 'reject') => {
    if (!token || !strike?.appeal || busy) return
    setBusy(true)
    try {
      await decideAppeal(token, strike.appeal.id, decision)
      toast.success(decision === 'approve' ? t({ id: 'eunha.strike_detail.appeal_approved', defaultMessage: 'Appeal approved.' }) : t({ id: 'eunha.strike_detail.appeal_rejected', defaultMessage: 'Appeal rejected.' }))
      load()
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setBusy(false)
    }
  }

  const s = strike
  const staff = can(permissions, 'manage_appeals')

  return (
    <div className="page-frame">
      <TopBar />
      {!token && <Button onClick={() => void beginLogin()}>{t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}</Button>}
      {error && <p className="text-destructive text-sm">{error}</p>}
      {token && !s && !error && <DetailSkeleton />}
      {s && (
        <div className="space-y-5">
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="min-w-0 flex-1 text-lg font-bold">
              {t({ id: 'mastodon.disputes.strikes.title', defaultMessage: '{action} from {date}' }, { action: STRIKE_TITLE_ACTIONS[s.action], date: formatDate(s.created_at) })}
            </h1>
            {staff && s.appeal?.state === 'pending' && (
              <>
                <Button size="sm" disabled={busy} onClick={() => void decide('approve')}>
                  {t({ id: 'mastodon.disputes.strikes.approve_appeal', defaultMessage: 'Approve appeal' })}
                </Button>
                <Button
                  size="sm"
                  variant="destructive"
                  disabled={busy}
                  onClick={() => void decide('reject')}
                >
                  {t({ id: 'mastodon.disputes.strikes.reject_appeal', defaultMessage: 'Reject appeal' })}
                </Button>
              </>
            )}
          </div>
          {s.overruled_at ? (
            <p className="text-sm text-green-700 dark:text-green-400">
              {t({ id: 'mastodon.disputes.strikes.appeal_approved', defaultMessage: 'This strike has been successfully appealed and is no longer valid' })}
            </p>
          ) : (
            s.appeal?.state === 'rejected' && (
              <p className="text-destructive text-sm">{t({ id: 'mastodon.disputes.strikes.appeal_rejected', defaultMessage: 'The appeal has been rejected' })}</p>
            )
          )}

          <dl className="grid gap-4 sm:grid-cols-2">
            <Detail label={t({ id: 'mastodon.disputes.strikes.created_at', defaultMessage: 'Dated' })}>{formatDate(s.created_at)}</Detail>
            <Detail label={t({ id: 'mastodon.disputes.strikes.recipient', defaultMessage: 'Addressed to' })}>
              {s.target_account &&
                (staff ? (
                  <Link to={`/admin/accounts/${s.target_account.id}`}>
                    {s.target_account.username}
                  </Link>
                ) : (
                  <Link to={`/@${s.target_account.acct}`}>{s.target_account.username}</Link>
                ))}
            </Detail>
            <Detail label={t({ id: 'mastodon.disputes.strikes.action_taken', defaultMessage: 'Action taken' })}>
              {s.overruled_at ? <del>{STRIKE_TITLES[s.action]}</del> : STRIKE_TITLES[s.action]}
            </Detail>
            {s.report_id && (
              <Detail label={t({ id: 'mastodon.disputes.strikes.associated_report', defaultMessage: 'Associated report' })}>
                <Link to={`/admin/reports/${s.report_id}`}>{t({ id: 'mastodon.admin.reports.report', defaultMessage: 'Report #{id}' }, { id: s.report_id })}</Link>
              </Detail>
            )}
            {s.appeal && (
              <Detail label={t({ id: 'mastodon.disputes.strikes.appeal_submitted_at', defaultMessage: 'Appeal submitted' })}>{formatDate(s.appeal.created_at)}</Detail>
            )}
          </dl>

          {s.text && <p className="text-sm whitespace-pre-wrap">{s.text}</p>}

          {s.statuses.length > 0 && (
            <section className="space-y-2">
              {s.statuses.map((status) => (
                <AdminStatus key={status.id} status={status} />
              ))}
            </section>
          )}

          {s.appeal ? (
            <section className="space-y-2">
              <h2 className="text-sm font-semibold">{t({ id: 'mastodon.disputes.strikes.appeal', defaultMessage: 'Appeal' })}</h2>
              <blockquote className="border-l-2 pl-3 text-sm whitespace-pre-wrap">
                {s.appeal.text}
              </blockquote>
            </section>
          ) : s.can_appeal ? (
            <form onSubmit={appeal} className="space-y-2">
              <h2 className="text-sm font-semibold">{t({ id: 'mastodon.disputes.strikes.appeals.submit', defaultMessage: 'Submit appeal' })}</h2>
              <Label htmlFor="appeal-text" className="sr-only">
                {t({ id: 'mastodon.disputes.strikes.appeal', defaultMessage: 'Appeal' })}
              </Label>
              <Textarea
                id="appeal-text"
                value={text}
                rows={4}
                maxLength={TEXT_LENGTH_LIMIT}
                className="resize-y"
                placeholder={t({ id: 'eunha.strike_detail.explain_why_this_decision_should_be', defaultMessage: 'Explain why this decision should be reversed.' })}
                onChange={(e) => setText(e.target.value)}
              />
              <p className="text-muted-foreground text-xs">
                {t({ id: 'eunha.strike_detail.appeal_until', defaultMessage: 'You can appeal until {date}.' }, { date: formatDate(s.appeal_deadline) })}
              </p>
              <Button type="submit" disabled={busy || !text.trim()}>
                {t({ id: 'mastodon.disputes.strikes.appeals.submit', defaultMessage: 'Submit appeal' })}
              </Button>
            </form>
          ) : (
            !s.appeal_eligible &&
            !staff && (
              <p className="text-muted-foreground text-sm">
                {t({ id: 'eunha.strike_detail.it_is_too_late_to_appeal', defaultMessage: 'It is too late to appeal this strike.' })}
              </p>
            )
          )}
          <p className="text-sm">
            <Link to="/disputes/strikes">{t({ id: 'eunha.strike_detail.all_strikes_against_your_account', defaultMessage: 'All strikes against your account' })}</Link>
          </p>
        </div>
      )}
    </div>
  )
}
