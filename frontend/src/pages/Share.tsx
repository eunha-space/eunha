import { useEffect } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'

import { beginLogin, getToken } from '../auth.ts'
import { Compose } from '@/components/compose.tsx'
import { TopBar } from '@/components/top-bar.tsx'

// Sharing a link or some text here, from a share button or the installed app's
// share target: Mastodon's `SharesController`, the composer already filled in.
export default function Share() {
  const [params] = useSearchParams()
  const navigate = useNavigate()
  const token = getToken()
  const text = ['title', 'text', 'url']
    .map((name) => params.get(name))
    .filter(Boolean)
    .join(' ')

  useEffect(() => {
    if (!token) void beginLogin(false, `${window.location.pathname}${window.location.search}`)
  }, [token])

  if (!token) return null

  return (
    <>
      <TopBar />
      <div className="page-frame">
        <Compose
          token={token}
          replyTo={null}
          initialText={text}
          onPosted={(status) => navigate(`/@${status.account.acct}/${status.id}`)}
        />
      </div>
    </>
  )
}
