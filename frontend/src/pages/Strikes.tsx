import { useEffect, useState } from 'react'

import { listStrikes, type Strike } from '../admin-api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { StrikeCard } from '@/components/admin/strike-card.tsx'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

/**
 * The strikes against the signed-in account: Mastodon's
 * `Disputes::StrikesController#index`, newest first. A frozen login still
 * reaches this page, as it does on Mastodon, so it can appeal.
 */
export default function Strikes() {
  const token = getToken()
  const [strikes, setStrikes] = useState<Strike[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!token) return
    listStrikes(token)
      .then(setStrikes)
      .catch((e) => setError(errorMessage(e)))
  }, [token])

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-1 text-lg font-bold">{t({ id: 'mastodon.relationships.status', defaultMessage: 'Account status' })}</h1>
      <p className="text-muted-foreground mb-4 text-sm">
        {t(
          { id: 'mastodon.disputes.strikes.description_html', defaultMessage: 'These are actions taken against your account and warnings that have been sent to you by the staff of {instance}.' },
          { instance: window.location.hostname },
        )}
      </p>
      {!token && (
        <Button onClick={() => void beginLogin()}>{t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}</Button>
      )}
      {error && <p className="text-destructive text-sm">{error}</p>}
      {token && strikes === null && !error && (
        <RowsSkeleton count={2} />
      )}
      {strikes?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.strikes.your_account_is_in_good_standing', defaultMessage: 'Your account is in good standing.' })}</p>
      )}
      <div className="space-y-2">
        {strikes?.map((s) => <StrikeCard key={s.id} strike={s} />)}
      </div>
    </div>
  )
}
