import { useEffect, useMemo, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { ImagePlus, Pencil } from 'lucide-react'

import { getCurrentAccount } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { restClient } from '../masto.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Spinner } from '@/components/ui/spinner.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { SectionSkeleton } from '@/components/skeletons.tsx'
import { errorMessage } from '@/lib/utils.ts'
import { t } from '@/i18n.tsx'

type UpdateCredentialsParams = NonNullable<
  Parameters<ReturnType<typeof restClient>['v1']['accounts']['updateCredentials']>[0]
>

const DISPLAY_NAME_MAX = 40
const NOTE_MAX = 500

/** Mastodon's placeholder for an account with no avatar or header. */
const nullIfMissing = (path: string | undefined) =>
  !path || path.endsWith('missing.png') ? null : path

interface Draft {
  displayName: string
  note: string
  avatar: string | null
  header: string | null
  discoverable: boolean
}

function ImageInput({
  kind,
  preview,
  disabled,
  onChange,
}: {
  kind: 'avatar' | 'header'
  preview: string | null
  disabled: boolean
  onChange: (file: File | undefined) => void
}) {
  const label =
    kind === 'avatar'
      ? t({ id: 'onboarding.profile.upload_avatar', defaultMessage: 'Upload profile picture' })
      : t({ id: 'onboarding.profile.upload_header', defaultMessage: 'Upload profile header' })
  const Icon = preview ? Pencil : ImagePlus
  return (
    <label
      title={label}
      className={
        kind === 'avatar'
          ? 'bg-muted border-background relative -mt-10 ml-4 flex size-20 cursor-pointer items-center justify-center overflow-hidden rounded-xl border-4'
          : 'bg-muted relative flex aspect-[3/1] w-full cursor-pointer items-center justify-center overflow-hidden rounded-lg'
      }
    >
      <input
        type="file"
        hidden
        accept="image/*"
        disabled={disabled}
        aria-label={label}
        onChange={(event) => onChange(event.currentTarget.files?.[0])}
      />
      {preview && <img src={preview} alt="" className="absolute inset-0 size-full object-cover" />}
      <span className="bg-background/70 relative rounded-full p-1.5">
        <Icon className="size-4" />
      </span>
    </label>
  )
}

/**
 * `/start/profile`, Mastodon's onboarding profile step (`Onboarding::Profile`):
 * a new account's name, bio, pictures, and whether it is discoverable, which
 * starts on. Discoverability is what lets other people find the account and
 * feature it in their collections (`interactionPolicy.canFeature`), so an
 * account that never passes through here stays out of both.
 */
export default function OnboardingProfile() {
  const token = getToken()
  const navigate = useNavigate()
  const [draft, setDraft] = useState<Draft | null>(null)
  const [avatar, setAvatar] = useState<File>()
  const [header, setHeader] = useState<File>()
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!token) {
      void beginLogin(false, '/start/profile')
      return
    }
    getCurrentAccount(token)
      .then((me) =>
        setDraft({
          displayName: me.displayName ?? '',
          note: me.source?.note ?? '',
          avatar: nullIfMissing(me.avatar),
          header: nullIfMissing(me.header),
          // `account?.discoverable ?? true`: on unless already chosen.
          discoverable: me.discoverable ?? true,
        }),
      )
      .catch((e) => setError(errorMessage(e)))
  }, [token])

  const avatarPreview = useMemo(
    () => (avatar ? URL.createObjectURL(avatar) : (draft?.avatar ?? null)),
    [avatar, draft?.avatar],
  )
  const headerPreview = useMemo(
    () => (header ? URL.createObjectURL(header) : (draft?.header ?? null)),
    [header, draft?.header],
  )

  const finish = async () => {
    if (!token || !draft) return
    setSaving(true)
    setError(null)
    try {
      // masto.js does not model `indexable`.
      const params: UpdateCredentialsParams & { indexable: boolean } = {
        displayName: draft.displayName,
        note: draft.note,
        avatar,
        header,
        discoverable: draft.discoverable,
        indexable: draft.discoverable,
      }
      await restClient(token).v1.accounts.updateCredentials(params)
      navigate('/', { replace: true })
    } catch (e) {
      setError(errorMessage(e))
      setSaving(false)
    }
  }

  const title = t({ id: 'onboarding.profile.title', defaultMessage: 'Profile setup' })

  return (
    <div className="page-frame">
      <h1 className="mb-3 text-lg font-bold">{title}</h1>
      {!draft ? (
        error ? (
          <p className="text-destructive text-sm">{error}</p>
        ) : (
          <SectionSkeleton rows={4} />
        )
      ) : (
        <div className="space-y-5">
          <div>
            <ImageInput kind="header" preview={headerPreview} disabled={saving} onChange={setHeader} />
            <ImageInput kind="avatar" preview={avatarPreview} disabled={saving} onChange={setAvatar} />
          </div>

          <div className="grid gap-1.5">
            <Label htmlFor="display_name">
              {t({ id: 'onboarding.profile.display_name', defaultMessage: 'Display name' })}
            </Label>
            <Input
              id="display_name"
              value={draft.displayName}
              maxLength={DISPLAY_NAME_MAX}
              disabled={saving}
              onChange={(event) => setDraft({ ...draft, displayName: event.currentTarget.value })}
            />
            <p className="text-muted-foreground text-xs">
              {t({ id: 'onboarding.profile.display_name_hint', defaultMessage: 'Your full name or your fun name…' })}
            </p>
          </div>

          <div className="grid gap-1.5">
            <Label htmlFor="note">{t({ id: 'onboarding.profile.note', defaultMessage: 'Bio' })}</Label>
            <Textarea
              id="note"
              value={draft.note}
              maxLength={NOTE_MAX}
              rows={4}
              disabled={saving}
              onChange={(event) => setDraft({ ...draft, note: event.currentTarget.value })}
            />
            <p className="text-muted-foreground text-xs">
              {t({ id: 'onboarding.profile.note_hint', defaultMessage: 'You can @mention other people or #hashtags…' })}
            </p>
          </div>

          <div className="flex items-start justify-between gap-4">
            <div className="space-y-0.5">
              <Label htmlFor="discoverable" className="text-sm">
                {t({ id: 'onboarding.profile.discoverable', defaultMessage: 'Make my profile discoverable' })}
                <span className="text-muted-foreground rounded border px-1 text-xs font-normal">
                  {t({ id: 'recommended', defaultMessage: 'Recommended' })}
                </span>
              </Label>
              <p className="text-muted-foreground text-xs">
                {t({
                  id: 'eunha.onboarding.discoverable_hint',
                  defaultMessage:
                    'When you opt in to discoverability, your posts may appear in search results and trending, your profile may be suggested to people with similar interests to you, and others may feature you in their collections.',
                })}
              </p>
            </div>
            <Switch
              id="discoverable"
              checked={draft.discoverable}
              disabled={saving}
              onCheckedChange={(on) => setDraft({ ...draft, discoverable: on })}
            />
          </div>

          {error && <p className="text-destructive text-sm">{error}</p>}

          <div className="flex justify-end">
            <Button onClick={() => void finish()} disabled={saving}>
              {saving && <Spinner />}
              {t({ id: 'onboarding.profile.finish', defaultMessage: 'Finish' })}
            </Button>
          </div>
        </div>
      )}
    </div>
  )
}
