import { useEffect, useState } from 'react'

import { getTermsOfServiceInterstitial, type TermsOfService } from '../api.ts'
import { getToken } from '../auth.ts'
import { formatPolicyDate } from '@/lib/policy-date.ts'
import { Button } from '@/components/ui/button.tsx'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog.tsx'

/**
 * Mastodon's `terms_of_service_interstitial/show`: for a user who was not
 * mailed about new terms — suspended, or away for over a year when they were
 * published — the terms stand in front of every page until they have opened
 * them. Like upstream's, it cannot be dismissed; opening the terms page is
 * what clears it.
 */
export function TermsOfServiceInterstitial() {
  const [terms, setTerms] = useState<TermsOfService | null>(null)

  useEffect(() => {
    const token = getToken()
    // The terms page itself is where the flag is cleared.
    if (!token || window.location.pathname.startsWith('/terms-of-service')) return
    getTermsOfServiceInterstitial(token)
      .then(setTerms)
      .catch(() => {})
  }, [])

  if (!terms) return null

  const domain = window.location.host
  return (
    <Dialog open>
      <DialogContent showCloseButton={false}>
        <DialogHeader>
          <DialogTitle>The terms of service of {domain} are changing</DialogTitle>
          <DialogDescription>
            {terms.effective ? (
              <>
                We have changed our terms of service since your last visit. We encourage
                you to review the updated terms.
              </>
            ) : (
              <>
                We're making some changes to our terms of service, which will be effective
                on <strong>{formatPolicyDate(terms.effective_date)}</strong>. We encourage
                you to review the updated terms.
              </>
            )}
          </DialogDescription>
        </DialogHeader>
        <p className="text-muted-foreground text-sm">
          By continuing to use {domain}, you are agreeing to these terms. If you disagree
          with the updated terms, you may terminate your agreement with {domain} at any time
          by deleting your account.
        </p>
        <DialogFooter>
          <Button render={<a href="/terms-of-service" />}>Review terms of service</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
