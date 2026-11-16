import { useEffect, useState } from 'react'

import { getPrivacyPolicy, type PrivacyPolicy } from '../api.ts'
import { PolicyText } from '@/components/policy-text.tsx'
import { TopBar } from '@/components/top-bar.tsx'
import { formatPolicyDate } from '@/lib/policy-date.ts'
import { TextSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

// Mastodon's `/privacy-policy`, which `urls.privacy_policy` points at. There is
// always one: the server falls back to the policy Mastodon ships.
export default function PrivacyPolicyPage() {
  const [policy, setPolicy] = useState<PrivacyPolicy | null | undefined>(undefined)

  useEffect(() => {
    getPrivacyPolicy()
      .then(setPolicy)
      .catch(() => setPolicy(null))
  }, [])

  return (
    <div className="page-frame">
      <TopBar title={t({ id: 'footer.privacy_policy', defaultMessage: 'Privacy policy' })} />
      {policy === undefined && <TextSkeleton />}
      {policy === null && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.privacy_policy.the_privacy_policy_could_not_be', defaultMessage: 'The privacy policy could not be loaded.' })}</p>
      )}
      {policy && (
        <div className="space-y-4">
          <div className="space-y-1">
            <h1 className="text-2xl font-bold">{t({ id: 'privacy_policy.title', defaultMessage: 'Privacy Policy' })}</h1>
            <p className="text-muted-foreground text-sm">
              {t({ id: 'privacy_policy.last_updated', defaultMessage: 'Last updated {date}' }, { date: formatPolicyDate(policy.updated_at) })}
            </p>
          </div>
          <PolicyText html={policy.content} />
        </div>
      )}
    </div>
  )
}
