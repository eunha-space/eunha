import { useEffect, useState } from 'react'
import { Link, useParams } from 'react-router-dom'

import { dismissTermsOfServiceInterstitial, getTermsOfService, type TermsOfService } from '../api.ts'
import { getToken } from '../auth.ts'
import { PolicyText } from '@/components/policy-text.tsx'
import { TopBar } from '@/components/top-bar.tsx'
import { formatPolicyDate } from '@/lib/policy-date.ts'

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
      <TopBar title="Terms of service" />
      {terms === undefined && <p className="text-muted-foreground text-sm">Loading…</p>}
      {terms === null && (
        <p className="text-muted-foreground text-sm">
          {date
            ? 'There are no terms of service effective on that date.'
            : 'This server has no terms of service.'}
        </p>
      )}
      {terms && (
        <div className="space-y-4">
          <div className="space-y-1">
            <h1 className="text-2xl font-bold">Terms of Service</h1>
            <p className="text-muted-foreground text-sm">
              {terms.effective
                ? `Last updated ${formatPolicyDate(terms.effective_date)}`
                : `Effective as of ${formatPolicyDate(terms.effective_date)}`}
              {terms.succeeded_by && (
                <>
                  {' · '}
                  <Link to={`/terms-of-service/${terms.succeeded_by}`} className="text-primary">
                    Upcoming changes on {formatPolicyDate(terms.succeeded_by)}
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
