import { useEffect, useState } from 'react'
import { Link, useParams } from 'react-router-dom'

import { dismissTermsOfServiceInterstitial, getTermsOfService, type TermsOfService } from '../api.ts'
import { getToken } from '../auth.ts'
import { PolicyText } from '@/components/policy-text.tsx'
import { TopBar } from '@/components/top-bar.tsx'
import { formatPolicyDate } from '@/lib/policy-date.ts'
import { TextSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

// Mastodon's `/terms-of-service` and `/terms-of-service/:date`: the current
// terms, or the version effective on a date, with a link forward when a later
// version has been published.
export default function TermsOfServicePage() {
  const { date } = useParams<{ date?: string }>()
  const [terms, setTerms] = useState<TermsOfService | null | undefined>(undefined)

  useEffect(() => {
    setTerms(undefined)
    getTermsOfService(date)
      .then(setTerms)
      .catch(() => setTerms(null))
  }, [date])

  // Opening the terms is what clears the interstitial, as Mastodon's
  // `TermsOfServiceController` does on every visit.
  useEffect(() => {
    const token = getToken()
    if (token) void dismissTermsOfServiceInterstitial(token)
  }, [])

  return (
    <div className="page-frame">
      <TopBar title={t({ id: 'footer.terms_of_service', defaultMessage: 'Terms of service' })} />
      {terms === undefined && <TextSkeleton />}
      {terms === null && (
        <p className="text-muted-foreground text-sm">
          {date
            ? t({ id: 'eunha.terms_of_service.there_are_no_terms_of_service', defaultMessage: 'There are no terms of service effective on that date.' })
            : t({ id: 'eunha.terms_of_service.this_server_has_no_terms_of', defaultMessage: 'This server has no terms of service.' })}
        </p>
      )}
      {terms && (
        <div className="space-y-4">
          <div className="space-y-1">
            <h1 className="text-2xl font-bold">{t({ id: 'terms_of_service.title', defaultMessage: 'Terms of Service' })}</h1>
            <p className="text-muted-foreground text-sm">
              {terms.effective
                ? t({ id: 'privacy_policy.last_updated', defaultMessage: 'Last updated {date}' }, { date: formatPolicyDate(terms.effective_date) })
                : t({ id: 'terms_of_service.effective_as_of', defaultMessage: 'Effective as of {date}' }, { date: formatPolicyDate(terms.effective_date) })}
              {terms.succeeded_by && (
                <>
                  {' · '}
                  <Link to={`/terms-of-service/${terms.succeeded_by}`} className="text-primary">
                    {t({ id: 'terms_of_service.upcoming_changes_on', defaultMessage: 'Upcoming changes on {date}' }, { date: formatPolicyDate(terms.succeeded_by) })}
                  </Link>
                </>
              )}
            </p>
          </div>
          <PolicyText html={terms.content} />
        </div>
      )}
    </div>
  )
}
