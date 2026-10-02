import { useEffect, useState } from 'react'

import { getPrivacyPolicy, type PrivacyPolicy } from '../api.ts'
import { PolicyText } from '@/components/policy-text.tsx'
import { TopBar } from '@/components/top-bar.tsx'
import { formatPolicyDate } from '@/lib/policy-date.ts'

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
      <TopBar title="Privacy policy" />
      {policy === undefined && <p className="text-muted-foreground text-sm">Loading…</p>}
      {policy === null && (
        <p className="text-muted-foreground text-sm">The privacy policy could not be loaded.</p>
      )}
      {policy && (
        <div className="space-y-4">
          <div className="space-y-1">
            <h1 className="text-2xl font-bold">Privacy Policy</h1>
            <p className="text-muted-foreground text-sm">
              Last updated {formatPolicyDate(policy.updated_at)}
            </p>
          </div>
          <PolicyText html={policy.content} />
        </div>
      )}
    </div>
  )
}
