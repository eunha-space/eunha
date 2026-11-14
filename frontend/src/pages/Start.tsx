import { useEffect, useState } from 'react'
import { useParams, useSearchParams } from 'react-router-dom'

import { beginLogin } from '../auth.ts'

/**
 * `/signup` and `/invite/:code`: the sign-up form is the server's, at
 * `/auth/signup`, as Mastodon's is, so this only sends the browser there with
 * the invite code. A full page load of `/invite/:code` never gets here; the
 * server redirects it.
 */
export function SignUpForm() {
  const [params] = useSearchParams()
  const { code } = useParams()
  useEffect(() => {
    const query = new URLSearchParams()
    const invite = code ?? params.get('invite')
    if (invite) query.set('invite', invite)
    const lang = params.get('lang')
    if (lang) query.set('lang', lang)
    const search = query.toString()
    window.location.replace(`/auth/signup${search ? `?${search}` : ''}`)
  }, [code, params])
  return <div className="page-frame" />
}

/**
 * `/start`, Mastodon's first stop after confirming an address: where signing
 * in on the confirmation result lands. The browser is signed in to the
 * account pages by then, so the web app's own sign-in goes straight through
 * the authorization page and comes back home.
 */
// Once per page load: registering the app twice would leave the browser
// holding the other registration's secret.
let starting: Promise<void> | null = null

export function Start() {
  const [error, setError] = useState<string | null>(null)
  useEffect(() => {
    starting ??= beginLogin(false, '/')
    starting.catch((e) => setError(String(e)))
  }, [])
  return (
    <div className="page-frame">
      <p className={error ? 'text-destructive text-sm' : 'text-muted-foreground text-sm'}>
        {error ?? 'Signing you in…'}
      </p>
    </div>
  )
}
