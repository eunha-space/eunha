// Account security: what Mastodon offers only as web forms (two-factor
// authentication, security keys, sessions, authorized apps, sign-in history),
// served by eunha under /api/eunha/v1/.

export class SecurityError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message)
    this.name = 'SecurityError'
  }
}

async function call<T>(
  path: string,
  token: string,
  method = 'GET',
  body?: unknown,
): Promise<T> {
  const res = await fetch(`${window.location.origin}${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  if (!res.ok) {
    let message = `Request failed (${res.status})`
    try {
      const json = (await res.json()) as { error?: string }
      if (json.error) message = json.error
    } catch {
      // keep the status-based message
    }
    throw new SecurityError(res.status, message)
  }
  return (await res.json()) as T
}

// ── Two-factor authentication ──────────────────────────────────────────────

export interface WebauthnCredential {
  id: string
  nickname: string
  created_at: string
}

export interface TwoFactorStatus {
  otp_enabled: boolean
  webauthn_enabled: boolean
  required: boolean
  recovery_codes_remaining: number
  webauthn_credentials: WebauthnCredential[]
  available: boolean
}

export interface OtpSetup {
  secret: string
  provisioning_uri: string
  qr_code: string
}

const TFA = '/api/eunha/v1/two_factor_authentication'

export const getTwoFactor = (token: string) => call<TwoFactorStatus>(TFA, token)

export const beginOtpSetup = (token: string, password: string) =>
  call<OtpSetup>(`${TFA}/otp`, token, 'POST', { password })

export const confirmOtpSetup = (token: string, otpAttempt: string) =>
  call<{ recovery_codes: string[] }>(`${TFA}/otp/confirm`, token, 'POST', {
    otp_attempt: otpAttempt,
  })

export const regenerateRecoveryCodes = (token: string, password: string) =>
  call<{ recovery_codes: string[] }>(`${TFA}/recovery_codes`, token, 'POST', { password })

export const disableTwoFactor = (token: string, password: string) =>
  call<TwoFactorStatus>(TFA, token, 'DELETE', { password })

export const removeSecurityKey = (token: string, id: string, password: string) =>
  call<TwoFactorStatus>(`${TFA}/webauthn_credentials/${id}`, token, 'DELETE', { password })

function b64urlToBuffer(value: string): ArrayBuffer {
  const base64 = value.replace(/-/g, '+').replace(/_/g, '/')
  const padded = base64 + '='.repeat((4 - (base64.length % 4)) % 4)
  const binary = atob(padded)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
  return bytes.buffer
}

function bufferToB64url(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer)
  let binary = ''
  for (const byte of bytes) binary += String.fromCharCode(byte)
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

interface CreationOptionsJSON {
  challenge: string
  timeout: number
  rp: { name: string; id?: string }
  user: { name: string; displayName: string; id: string }
  pubKeyCredParams: { type: 'public-key'; alg: number }[]
  excludeCredentials: { type: 'public-key'; id: string }[]
  authenticatorSelection: AuthenticatorSelectionCriteria
}

export const securityKeysSupported = () =>
  typeof window !== 'undefined' && 'PublicKeyCredential' in window

/** Make a new security key with the browser and register it. */
export async function addSecurityKey(
  token: string,
  password: string,
  nickname: string,
): Promise<WebauthnCredential> {
  const options = await call<CreationOptionsJSON>(
    `${TFA}/webauthn_credentials/options`,
    token,
    'POST',
    { password },
  )
  const credential = (await navigator.credentials.create({
    publicKey: {
      challenge: b64urlToBuffer(options.challenge),
      timeout: options.timeout,
      rp: options.rp,
      user: {
        name: options.user.name,
        displayName: options.user.displayName,
        id: b64urlToBuffer(options.user.id),
      },
      pubKeyCredParams: options.pubKeyCredParams,
      excludeCredentials: options.excludeCredentials.map((c) => ({
        type: c.type,
        id: b64urlToBuffer(c.id),
      })),
      authenticatorSelection: options.authenticatorSelection,
    },
  })) as PublicKeyCredential | null
  if (!credential) throw new SecurityError(0, 'No security key was created')
  const response = credential.response as AuthenticatorAttestationResponse
  return call<WebauthnCredential>(`${TFA}/webauthn_credentials`, token, 'POST', {
    nickname,
    credential: {
      id: credential.id,
      rawId: bufferToB64url(credential.rawId),
      type: credential.type,
      response: {
        clientDataJSON: bufferToB64url(response.clientDataJSON),
        attestationObject: bufferToB64url(response.attestationObject),
      },
    },
  })
}

// ── Sessions, authorized applications, sign-in history ─────────────────────

export interface Session {
  id: string
  ip: string | null
  user_agent: string
  browser: string
  platform: string
  description: string
  created_at: string
  updated_at: string
  current: boolean
}

export interface AuthorizedApplication {
  id: string
  name: string
  website: string | null
  scopes: string[]
  superapp: boolean
  last_used_at: string | null
  created_at: string
}

export interface LoginActivity {
  id: string
  authentication_method: string | null
  provider: string | null
  success: boolean
  failure_reason: string | null
  ip: string | null
  user_agent: string | null
  browser: string
  platform: string
  created_at: string | null
}

export const getSessions = (token: string) => call<Session[]>('/api/eunha/v1/sessions', token)

export const revokeSession = (token: string, id: string) =>
  call<object>(`/api/eunha/v1/sessions/${id}`, token, 'DELETE')

export const getAuthorizedApplications = (token: string) =>
  call<AuthorizedApplication[]>('/api/eunha/v1/authorized_applications', token)

export const revokeAuthorizedApplication = (token: string, id: string) =>
  call<object>(`/api/eunha/v1/authorized_applications/${id}`, token, 'DELETE')

export const getLoginActivities = (token: string, maxId?: string) =>
  call<LoginActivity[]>(
    `/api/eunha/v1/login_activities${maxId ? `?max_id=${encodeURIComponent(maxId)}` : ''}`,
    token,
  )

// ── Preferences Mastodon keeps on its web settings pages ───────────────────

export interface Preferences {
  noindex: boolean
  show_application: boolean
  chosen_languages: string[] | null
  locale: string | null
  time_zone: string | null
  notification_emails: Record<string, boolean | string | null>
}

/** One of Rails' `ActiveSupport::TimeZone.all`, as the settings page lists it. */
export interface TimeZoneChoice {
  value: string
  label: string
}

export const getTimeZones = (token: string) =>
  call<TimeZoneChoice[]>('/api/eunha/v1/preferences/time_zones', token)

export const getPreferences = (token: string) =>
  call<Preferences>('/api/eunha/v1/preferences', token)

export const updatePreferences = (
  token: string,
  changes: Partial<Omit<Preferences, 'notification_emails'>> & {
    notification_emails?: Record<string, boolean | string>
  },
) => call<Preferences>('/api/eunha/v1/preferences', token, 'PATCH', changes)
