import { useEffect, useState } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'

import { search } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'

// Where another server sends someone who wants to follow an account or answer
// a post from their account here. Mastodon's `AuthorizeInteractionsController`,
// which asks the visitor to sign in, looks up what was named, and shows it.
export default function AuthorizeInteraction() {
  const [params] = useSearchParams()
  const navigate = useNavigate()
  const [failed, setFailed] = useState(false)
  const target = params.get('uri') ?? (params.get('acct') ?? '').replace(/^acct:/, '')

  useEffect(() => {
    if (!target) {
      setFailed(true)
      return
    }
    const token = getToken()
    if (!token) {
      void beginLogin(false, `${window.location.pathname}${window.location.search}`)
      return
    }
    let cancelled = false
    search(target, token)
      .then((results) => {
        if (cancelled) return
        const status = results.statuses[0]
        const account = results.accounts[0]
        if (status) navigate(`/@${status.account.acct}/${status.id}`, { replace: true })
        else if (account) navigate(`/@${account.acct}`, { replace: true })
        else setFailed(true)
      })
      .catch(() => {
        if (!cancelled) setFailed(true)
      })
    return () => {
      cancelled = true
    }
  }, [target, navigate])

  return (
    <>
      <TopBar />
      <div className="page-frame">
        <p className={failed ? 'text-destructive text-sm' : 'text-muted-foreground text-sm'}>
          {failed ? 'Nothing was found at that address.' : 'Looking that up…'}
        </p>
      </div>
    </>
  )
}
