import { useCallback, useEffect, useState } from 'react'
import { KeyRound, ShieldCheck, Trash2 } from 'lucide-react'
import { toast } from 'sonner'

import {
  addSecurityKey,
  beginOtpSetup,
  confirmOtpSetup,
  disableTwoFactor,
  getTwoFactor,
  regenerateRecoveryCodes,
  removeSecurityKey,
  securityKeysSupported,
  SecurityError,
  type OtpSetup,
  type TwoFactorStatus,
} from '../security-api.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { SectionSkeleton } from '@/components/skeletons.tsx'
import { currentLocale, t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

const message = (e: unknown, fallback: string) =>
  e instanceof SecurityError ? e.message : fallback

function RecoveryCodes({ codes, onDone }: { codes: string[]; onDone: () => void }) {
  return (
    <div className="space-y-2">
      <p className="text-muted-foreground text-sm">
        <FormattedMessage
          id='mastodon.two_factor_authentication.recovery_instructions_html'
          defaultMessage='If you ever lose access to your phone, you can use one of the recovery codes below to regain access to your account. <strong>Keep the recovery codes safe</strong>. For example, you may print them and store them with other important documents.'
          values={{ strong: (chunks) => <strong>{chunks}</strong> }}
        />
      </p>
      <ol className="grid list-decimal grid-cols-2 gap-x-6 pl-6 font-mono text-sm">
        {codes.map((code) => (
          <li key={code}>{code}</li>
        ))}
      </ol>
      <Button size="sm" variant="secondary" onClick={onDone}>
        {t({ id: 'lists.done', defaultMessage: 'Done' })}
      </Button>
    </div>
  )
}

/**
 * Mastodon's two-factor authentication settings: an authenticator app with
 * its recovery codes, and security keys on top of it.
 */
export function TwoFactorSettings({ token }: { token: string }) {
  const [status, setStatus] = useState<TwoFactorStatus | null>(null)
  const [failed, setFailed] = useState(false)
  const [password, setPassword] = useState('')
  const [setup, setSetup] = useState<OtpSetup | null>(null)
  const [code, setCode] = useState('')
  const [codes, setCodes] = useState<string[] | null>(null)
  const [nickname, setNickname] = useState('')
  const [busy, setBusy] = useState(false)

  const reload = useCallback(() => {
    getTwoFactor(token)
      .then(setStatus)
      .catch(() => setFailed(true))
  }, [token])

  useEffect(reload, [reload])

  if (!status) return failed ? null : <SectionSkeleton rows={2} />

  const run = async (work: () => Promise<void>, fallback: string) => {
    setBusy(true)
    try {
      await work()
    } catch (e) {
      toast.error(message(e, fallback))
    } finally {
      setBusy(false)
    }
  }

  const passwordField = (
    <div className="space-y-1">
      <Label htmlFor="two-factor-password">{t({ id: 'mastodon.simple_form.labels.defaults.current_password', defaultMessage: 'Current password' })}</Label>
      <Input
        id="two-factor-password"
        type="password"
        autoComplete="current-password"
        value={password}
        onChange={(e) => setPassword(e.target.value)}
      />
    </div>
  )

  return (
    <section className="space-y-3 rounded-lg border p-4">
      <div>
        <h2 className="font-semibold">{t({ id: 'eunha.two_factor_settings.two_factor_authentication', defaultMessage: 'Two-factor authentication' })}</h2>
        {status.required && !status.otp_enabled && !status.webauthn_enabled && (
          <p className="text-destructive text-sm">
            {t(
              { id: 'mastodon.two_factor_authentication.role_requirement', defaultMessage: '{domain} requires you to set up Two-Factor Authentication before you can use Mastodon.' },
              { domain: window.location.hostname },
            )}
          </p>
        )}
      </div>

      {codes ? (
        <RecoveryCodes
          codes={codes}
          onDone={() => {
            setCodes(null)
            reload()
          }}
        />
      ) : setup ? (
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault()
            void run(async () => {
              const result = await confirmOtpSetup(token, code.trim())
              toast.success(t({ id: 'mastodon.two_factor_authentication.enabled_success', defaultMessage: 'Two-factor authentication successfully enabled' }))
              setSetup(null)
              setCode('')
              setCodes(result.recovery_codes)
            }, t({ id: 'mastodon.otp_authentication.wrong_code', defaultMessage: 'The entered code was invalid! Are server time and device time correct?' }))
          }}
        >
          <p className="text-muted-foreground text-sm">
            <FormattedMessage
              id='mastodon.otp_authentication.instructions_html'
              defaultMessage='<strong>Scan this QR code into Google Authenticator or a similar TOTP app on your phone</strong>. From now on, that app will generate tokens that you will have to enter when logging in.'
              values={{ strong: (chunks) => <strong>{chunks}</strong> }}
            />
          </p>
          {/* The SVG is generated by the server from the provisioning URI. */}
          <div
            className="w-48 rounded bg-white p-2"
            dangerouslySetInnerHTML={{ __html: setup.qr_code }}
          />
          <p className="text-muted-foreground text-sm">
            {t({ id: 'mastodon.otp_authentication.manual_instructions', defaultMessage: 'If you can\'\'t scan the QR code and need to enter it manually, here is the plain-text secret:' })}
          </p>
          <p className="font-mono text-sm break-all">{setup.secret}</p>
          <div className="space-y-1">
            <Label htmlFor="two-factor-code">
              {t({ id: 'mastodon.otp_authentication.code_hint', defaultMessage: 'Enter the code generated by your authenticator app to confirm' })}
            </Label>
            <Input
              id="two-factor-code"
              inputMode="numeric"
              autoComplete="one-time-code"
              value={code}
              onChange={(e) => setCode(e.target.value)}
              required
            />
          </div>
          <div className="flex gap-2">
            <Button type="submit" size="sm" disabled={busy || !code}>
              {t({ id: 'mastodon.otp_authentication.enable', defaultMessage: 'Enable' })}
            </Button>
            <Button type="button" size="sm" variant="ghost" onClick={() => setSetup(null)}>
              {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
            </Button>
          </div>
        </form>
      ) : !status.otp_enabled ? (
        <div className="space-y-3">
          <p className="text-muted-foreground text-sm">
            {t({ id: 'eunha.two_factor_settings.if_you_enable_two_factor_authentication', defaultMessage: 'If you enable two-factor authentication using an authenticator app, logging in will require you to be in possession of your phone, which will generate tokens for you to enter.' })}
          </p>
          {status.available ? (
            <>
              {passwordField}
              <Button
                size="sm"
                disabled={busy || !password}
                onClick={() =>
                  void run(async () => {
                    setSetup(await beginOtpSetup(token, password))
                    setPassword('')
                  }, t({ id: 'eunha.two_factor_settings.could_not_start_the_setup', defaultMessage: 'Could not start the setup' }))
                }
              >
                <ShieldCheck /> {t({ id: 'mastodon.otp_authentication.setup', defaultMessage: 'Set up' })}
              </Button>
            </>
          ) : (
            <p className="text-sm">{t({ id: 'eunha.two_factor_settings.two_factor_authentication_is_not_available', defaultMessage: 'Two-factor authentication is not available on this server.' })}</p>
          )}
        </div>
      ) : (
        <div className="space-y-4">
          <p className="text-sm">
            {t(
              { id: 'eunha.two_factor_settings.enabled_with_codes_left', defaultMessage: 'Two-factor authentication is enabled · {count, plural, one {# recovery code} other {# recovery codes}} left' },
              { count: status.recovery_codes_remaining },
            )}
          </p>
          <p className="text-muted-foreground text-sm">
            {t({ id: 'mastodon.two_factor_authentication.lost_recovery_codes', defaultMessage: 'Recovery codes allow you to regain access to your account if you lose your phone. If you\'\'ve lost your recovery codes, you can regenerate them here. Your old recovery codes will be invalidated.' })}
          </p>
          {passwordField}
          <div className="flex flex-wrap gap-2">
            <Button
              size="sm"
              variant="secondary"
              disabled={busy || !password}
              onClick={() =>
                void run(async () => {
                  const result = await regenerateRecoveryCodes(token, password)
                  toast.success(t({ id: 'mastodon.two_factor_authentication.recovery_codes_regenerated', defaultMessage: 'Recovery codes successfully regenerated' }))
                  setPassword('')
                  setCodes(result.recovery_codes)
                }, t({ id: 'eunha.two_factor_settings.could_not_generate_recovery_codes', defaultMessage: 'Could not generate recovery codes' }))
              }
            >
              {t({ id: 'mastodon.two_factor_authentication.generate_recovery_codes', defaultMessage: 'Generate recovery codes' })}
            </Button>
            <Button
              size="sm"
              variant="destructive"
              disabled={busy || !password}
              onClick={() =>
                void run(async () => {
                  setStatus(await disableTwoFactor(token, password))
                  setPassword('')
                  toast.success(t({ id: 'mastodon.two_factor_authentication.disabled_success', defaultMessage: 'Two-factor authentication successfully disabled' }))
                }, t({ id: 'eunha.two_factor_settings.could_not_disable_two_factor_authentication', defaultMessage: 'Could not disable two-factor authentication' }))
              }
            >
              {t({ id: 'mastodon.two_factor_authentication.disable', defaultMessage: 'Disable 2FA' })}
            </Button>
          </div>

          <div className="space-y-2">
            <h3 className="text-sm font-semibold">{t({ id: 'mastodon.settings.webauthn_authentication', defaultMessage: 'Security keys' })}</h3>
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.two_factor_settings.if_you_enable_security_key_authentication', defaultMessage: 'If you enable security key authentication, logging in will require you to use one of your security keys.' })}
            </p>
            {status.webauthn_credentials.length > 0 && (
              <ul className="divide-y rounded border text-sm">
                {status.webauthn_credentials.map((key) => (
                  <li key={key.id} className="flex items-center justify-between gap-2 p-2">
                    <span>
                      <KeyRound className="mr-1 inline size-4" />
                      {key.nickname}
                      <span className="text-muted-foreground">
                        {' '}
                        ·{' '}
                        {t({ id: 'mastodon.webauthn_credentials.registered_on', defaultMessage: 'Registered on {date}' }, { date: new Date(key.created_at).toLocaleDateString(currentLocale()) })}
                      </span>
                    </span>
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={t({ id: 'eunha.two_factor_settings.delete', defaultMessage: 'Delete {nickname}' }, { nickname: key.nickname })}
                      disabled={busy || !password}
                      onClick={() =>
                        void run(async () => {
                          setStatus(await removeSecurityKey(token, key.id, password))
                          toast.success(t({ id: 'mastodon.webauthn_credentials.destroy.success', defaultMessage: 'Your security key was successfully deleted.' }))
                        }, t({ id: 'mastodon.webauthn_credentials.destroy.error', defaultMessage: 'There was a problem deleting you security key. Please try again.' }))
                      }
                    >
                      <Trash2 />
                    </Button>
                  </li>
                ))}
              </ul>
            )}
            {securityKeysSupported() ? (
              <form
                className="flex flex-wrap items-end gap-2"
                onSubmit={(e) => {
                  e.preventDefault()
                  void run(async () => {
                    await addSecurityKey(token, password, nickname.trim())
                    toast.success(t({ id: 'mastodon.webauthn_credentials.create.success', defaultMessage: 'Your security key was successfully added.' }))
                    setNickname('')
                    reload()
                  }, t({ id: 'mastodon.webauthn_credentials.create.error', defaultMessage: 'There was a problem adding your security key. Please try again.' }))
                }}
              >
                <div className="space-y-1">
                  <Label htmlFor="security-key-nickname">{t({ id: 'mastodon.webauthn_credentials.nickname', defaultMessage: 'Nickname' })}</Label>
                  <Input
                    id="security-key-nickname"
                    placeholder={t({ id: 'mastodon.webauthn_credentials.nickname_hint', defaultMessage: 'Enter the nickname of your new security key' })}
                    value={nickname}
                    onChange={(e) => setNickname(e.target.value)}
                    required
                  />
                </div>
                <Button type="submit" size="sm" disabled={busy || !password || !nickname.trim()}>
                  {t({ id: 'mastodon.webauthn_credentials.add', defaultMessage: 'Add new security key' })}
                </Button>
              </form>
            ) : (
              <p className="text-muted-foreground text-sm">
                {t({ id: 'mastodon.webauthn_credentials.not_supported', defaultMessage: 'This browser doesn\'\'t support security keys' })}
              </p>
            )}
          </div>
        </div>
      )}
    </section>
  )
}
