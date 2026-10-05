import { useEffect, useState } from 'react'
import { useSearchParams } from 'react-router-dom'
import { completeLogin } from '../auth.ts'

const logins = new Map<string, Promise<void>>()

function loginOnce(code: string) {
  let login = logins.get(code)
  if (!login) {
    login = completeLogin(code)
    logins.set(code, login)
  }
  return login
}

export default function Callback() {
  const [params] = useSearchParams()
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    const code = params.get('code')
    if (!code) {
      setError(params.get('error_description') ?? 'No authorization code returned.')
      return
    }
    loginOnce(code)
      .then(() => window.location.replace('/'))
      .catch((e) => setError(String(e)))
  }, [params])

  return (
    <div className="page-frame">
      <p className={error ? 'text-destructive text-sm' : 'text-muted-foreground text-sm'}>
        {error ?? 'Signing you in…'}
      </p>
    </div>
  )
}
