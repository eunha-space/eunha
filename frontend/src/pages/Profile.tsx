import { useCallback, useEffect, useRef, useState } from 'react'
import { Link, Navigate, useLocation, useParams } from 'react-router-dom'
import { toast } from 'sonner'
import {
  Ban,
  CheckCircle2,
  ExternalLink,
  Flag,
  MessageCircle,
  ImageUp,
  MoreHorizontal,
  Pencil,
  Pin,
  Trash2,
  UserPlus,
  VolumeX,
} from 'lucide-react'

import type { mastodon } from '../masto.ts'
import {
  createFeaturedTag,
  deleteFeaturedTag,
  deleteProfileAvatar,
  deleteProfileHeader,
  getAccountFeaturedTags,
  getAccountStatuses,
  getCurrentAccount,
  getFeaturedTags,
  getFeaturedTagSuggestions,
  getPinnedStatuses,
  getRelationship,
  lookupAccount,
  setAccountNote,
  setBlock,
  setFollow,
  setMute,
  updateAccountImages,
  updateAccountProfile,
} from '../api.ts'
import { getToken } from '../auth.ts'
import { withoutMessages } from '../lib/statuses.ts'
import { useInfiniteFeed } from '../hooks/use-infinite-feed.ts'
import { usePendingFollow } from '@/hooks/use-pending-follow.ts'
import { FollowButtonIcon } from '@/components/follow-button.tsx'
import { TopBar } from '@/components/top-bar.tsx'
import {
  ActivityFilterBar,
  FeaturedPane,
  MediaGrid,
  offeredViews,
  ProfileTabs,
  profileView,
  useActivityFilters,
} from '@/components/profile-tabs.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { ReportDialog } from '@/components/report-dialog.tsx'
import { useComposeModal } from '@/components/compose-modal.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'
import { EmailSubscriptionForm } from '@/components/email-subscription-form.tsx'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog.tsx'
import { errorMessage } from '@/lib/utils.ts'
import { AnimateEmoji, DisplayName, EmojiHtml, EmojiText } from '@/components/emoji.tsx'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { Button } from '@/components/ui/button.tsx'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { useAnimatedHeader, useAnimatedImage } from '@/hooks/use-animated-image.ts'
import { FeedSkeleton, LinesSkeleton, ProfileSkeleton } from '@/components/skeletons.tsx'
import { currentLocale, t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

// Limits mirror Mastodon's server-side validations (app/models/account.rb,
// app/models/featured_tag.rb): display name ≤ 40, note ≤ 500, at most 4 custom
// fields of ≤ 255 chars each, and ≤ 10 featured hashtags.
const DISPLAY_NAME_MAX = 40
const NOTE_MAX = 500
const FIELD_MAX = 4
const FIELD_CHARS = 255
const FEATURED_TAG_MAX = 10

type EditField = { name: string; value: string }

function ProfileEditModal({
  token,
  initialDisplayName,
  initialNote,
  initialFields,
  onCancel,
  onSaved,
}: {
  token: string
  initialDisplayName: string
  initialNote: string
  initialFields: EditField[]
  onCancel: () => void
  onSaved: (updated: mastodon.v1.AccountCredentials) => void
}) {
  const [displayName, setDisplayName] = useState(initialDisplayName)
  const [note, setNote] = useState(initialNote)
  const [fields, setFields] = useState<EditField[]>(
    initialFields.slice(0, FIELD_MAX),
  )
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Featured hashtags are managed through their own endpoints and take effect
  // immediately (add/remove), independent of the Save button.
  const [featured, setFeatured] = useState<mastodon.v1.FeaturedTag[] | null>(null)
  const [suggestions, setSuggestions] = useState<mastodon.v1.Tag[]>([])
  const [newTag, setNewTag] = useState('')
  const [tagBusy, setTagBusy] = useState(false)
  const [tagError, setTagError] = useState<string | null>(null)

  useEffect(() => {
    getFeaturedTags(token)
      .then(setFeatured)
      .catch(() => setFeatured([]))
    getFeaturedTagSuggestions(token)
      .then(setSuggestions)
      .catch(() => {})
  }, [token])

  const setField = (index: number, key: keyof EditField, value: string) =>
    setFields((cur) =>
      cur.map((f, i) => (i === index ? { ...f, [key]: value } : f)),
    )
  const addField = () =>
    setFields((cur) =>
      cur.length >= FIELD_MAX ? cur : [...cur, { name: '', value: '' }],
    )
  const removeField = (index: number) =>
    setFields((cur) => cur.filter((_, i) => i !== index))

  const save = async () => {
    setBusy(true)
    setError(null)
    try {
      // Drop rows that are blank on both sides, mirroring Mastodon. Send at
      // least one blank row so the server clears fields when all were removed.
      const cleaned = fields
        .map((f) => ({ name: f.name.trim(), value: f.value.trim() }))
        .filter((f) => f.name !== '' || f.value !== '')
      const updated = await updateAccountProfile(token, {
        displayName,
        note,
        fieldsAttributes: cleaned.length > 0 ? cleaned : [{ name: '', value: '' }],
      })
      onSaved(updated)
    } catch (e) {
      setError(errorMessage(e))
      setBusy(false)
    }
  }

  const addTag = async (name: string) => {
    const clean = name.trim().replace(/^#/, '')
    if (!clean || tagBusy) return
    setTagBusy(true)
    setTagError(null)
    try {
      const created = await createFeaturedTag(token, clean)
      setFeatured((cur) =>
        cur ? [created, ...cur.filter((t) => t.id !== created.id)] : [created],
      )
      setNewTag('')
    } catch (e) {
      setTagError(errorMessage(e))
    } finally {
      setTagBusy(false)
    }
  }

  const removeTag = async (id: string) => {
    if (tagBusy) return
    setTagBusy(true)
    setTagError(null)
    try {
      await deleteFeaturedTag(token, id)
      setFeatured((cur) => cur?.filter((t) => t.id !== id) ?? null)
    } catch (e) {
      setTagError(errorMessage(e))
    } finally {
      setTagBusy(false)
    }
  }

  const featuredNames = new Set((featured ?? []).map((t) => t.name.toLowerCase()))
  const openSuggestions = suggestions.filter(
    (t) => !featuredNames.has(t.name.toLowerCase()),
  )
  const atTagLimit = (featured?.length ?? 0) >= FEATURED_TAG_MAX

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/50 px-3 py-10 sm:py-16">
      <div className="bg-card text-card-foreground my-auto w-full max-w-xl rounded-md border shadow-lg">
        <div className="flex items-center justify-between border-b px-4 py-3">
          <h2 className="text-sm font-semibold">{t({ id: 'account.edit_profile', defaultMessage: 'Edit profile' })}</h2>
          <Button variant="ghost" size="sm" onClick={onCancel} disabled={busy}>
            {t({ id: 'lightbox.close', defaultMessage: 'Close' })}
          </Button>
        </div>
        <div className="space-y-5 p-4">
          <label className="grid gap-2 text-sm">
            <span className="text-muted-foreground">{t({ id: 'account_edit.display_name.title', defaultMessage: 'Display name' })}</span>
            <Input
              value={displayName}
              maxLength={DISPLAY_NAME_MAX}
              disabled={busy}
              onChange={(event) => setDisplayName(event.currentTarget.value)}
              placeholder={t({ id: 'eunha.profile.your_name', defaultMessage: 'Your name' })}
            />
            <span className="text-muted-foreground text-right text-xs">
              {DISPLAY_NAME_MAX - displayName.length}
            </span>
          </label>

          <label className="grid gap-2 text-sm">
            <span className="text-muted-foreground">{t({ id: 'collections.collection_description', defaultMessage: 'Description' })}</span>
            <Textarea
              value={note}
              maxLength={NOTE_MAX}
              rows={5}
              disabled={busy}
              onChange={(event) => setNote(event.currentTarget.value)}
              placeholder={t({ id: 'eunha.profile.tell_people_about_yourself', defaultMessage: 'Tell people about yourself' })}
            />
            <span className="text-muted-foreground text-right text-xs">
              {NOTE_MAX - note.length}
            </span>
          </label>

          <div className="grid gap-2 text-sm">
            <div className="flex items-center justify-between">
              <span className="text-muted-foreground">{t({ id: 'eunha.profile.profile_metadata', defaultMessage: 'Profile metadata' })}</span>
              <span className="text-muted-foreground text-xs">
                {fields.length}/{FIELD_MAX}
              </span>
            </div>
            {fields.map((field, index) => (
              <div key={index} className="flex gap-2">
                <Input
                  value={field.name}
                  maxLength={FIELD_CHARS}
                  disabled={busy}
                  onChange={(event) =>
                    setField(index, 'name', event.currentTarget.value)
                  }
                  placeholder={t({ id: 'account_edit.field_edit_modal.name_label', defaultMessage: 'Label' })}
                  className="flex-1"
                />
                <Input
                  value={field.value}
                  maxLength={FIELD_CHARS}
                  disabled={busy}
                  onChange={(event) =>
                    setField(index, 'value', event.currentTarget.value)
                  }
                  placeholder={t({ id: 'mastodon.simple_form.labels.account.fields.value', defaultMessage: 'Content' })}
                  className="flex-1"
                />
                <Button
                  variant="ghost"
                  size="icon"
                  disabled={busy}
                  onClick={() => removeField(index)}
                  aria-label={t({ id: 'eunha.profile.remove_field', defaultMessage: 'Remove field' })}
                >
                  <Trash2 />
                </Button>
              </div>
            ))}
            {fields.length < FIELD_MAX && (
              <Button
                variant="outline"
                size="sm"
                disabled={busy}
                onClick={addField}
                className="justify-self-start"
              >
                {t({ id: 'account_edit.custom_fields.add_label', defaultMessage: 'Add field' })}
              </Button>
            )}
          </div>

          {error && <p className="text-destructive text-sm">{error}</p>}
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={onCancel} disabled={busy}>
              {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
            </Button>
            <Button onClick={() => void save()} disabled={busy}>
              {t({ id: 'lists.save', defaultMessage: 'Save' })}
            </Button>
          </div>

          <div className="grid gap-2 border-t pt-4 text-sm">
            <span className="text-muted-foreground">{t({ id: 'account_edit.featured_hashtags.title', defaultMessage: 'Featured hashtags' })}</span>
            {featured === null ? (
              <LinesSkeleton count={1} className="w-48" />
            ) : (
              <>
                {featured.length > 0 && (
                  <ul className="flex flex-wrap gap-2">
                    {featured.map((tag) => (
                      <li
                        key={tag.id}
                        className="bg-muted flex items-center gap-1 rounded-full px-3 py-1"
                      >
                        <span>#{tag.name}</span>
                        <button
                          type="button"
                          className="text-muted-foreground hover:text-foreground disabled:opacity-50"
                          disabled={tagBusy}
                          onClick={() => void removeTag(tag.id)}
                          aria-label={t({ id: 'eunha.profile.remove', defaultMessage: 'Remove #{name}' }, { name: tag.name })}
                        >
                          <Trash2 className="size-3.5" />
                        </button>
                      </li>
                    ))}
                  </ul>
                )}
                {!atTagLimit && (
                  <div className="flex gap-2">
                    <Input
                      value={newTag}
                      disabled={tagBusy}
                      onChange={(event) => setNewTag(event.currentTarget.value)}
                      onKeyDown={(event) => {
                        if (event.key === 'Enter') {
                          event.preventDefault()
                          void addTag(newTag)
                        }
                      }}
                      placeholder={t({ id: 'eunha.profile.add_a_hashtag', defaultMessage: 'Add a hashtag' })}
                      className="flex-1"
                    />
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={tagBusy || newTag.trim() === ''}
                      onClick={() => void addTag(newTag)}
                    >
                      {t({ id: 'lists.add_member', defaultMessage: 'Add' })}
                    </Button>
                  </div>
                )}
                {atTagLimit && (
                  <p className="text-muted-foreground text-xs">
                    {t({ id: 'eunha.profile.featured_tag_limit', defaultMessage: 'You\'ve reached the limit of {count} featured hashtags.' }, { count: FEATURED_TAG_MAX })}
                  </p>
                )}
                {!atTagLimit && openSuggestions.length > 0 && (
                  <div className="flex flex-wrap gap-2">
                    {openSuggestions.map((tag) => (
                      <button
                        key={tag.name}
                        type="button"
                        className="text-primary text-xs hover:underline disabled:opacity-50"
                        disabled={tagBusy}
                        onClick={() => void addTag(tag.name)}
                      >
                        #{tag.name}
                      </button>
                    ))}
                  </div>
                )}
                {tagError && <p className="text-destructive text-xs">{tagError}</p>}
              </>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

function hasCustomHeader(header: string | null | undefined) {
  return !!header && !header.includes('/headers/original/missing')
}

type ImageKind = 'avatar' | 'header'

interface CropDraft {
  kind: ImageKind
  file: File
  url: string
}

function cropConfig(kind: ImageKind) {
  return kind === 'avatar'
    ? { aspect: 1, width: 400, height: 400 }
    : { aspect: 3, width: 1500, height: 500 }
}

function ProfileImageCropModal({
  draft,
  onCancel,
  onComplete,
}: {
  draft: CropDraft
  onCancel: () => void
  onComplete: (file: File) => void
}) {
  const config = cropConfig(draft.kind)
  const frameRef = useRef<HTMLDivElement>(null)
  const imageRef = useRef<HTMLImageElement>(null)
  const dragRef = useRef<{ pointerId: number; x: number; y: number } | null>(null)
  const [imageSize, setImageSize] = useState({ width: 0, height: 0 })
  const [pan, setPan] = useState({ x: 0, y: 0 })
  const [zoom, setZoom] = useState(1)
  const [busy, setBusy] = useState(false)

  const clampPan = (next: { x: number; y: number }, nextZoom = zoom) => {
    const frame = frameRef.current
    if (!frame || imageSize.width === 0 || imageSize.height === 0) return next

    const frameWidth = frame.clientWidth
    const frameHeight = frame.clientHeight
    const scale = Math.max(
      frameWidth / imageSize.width,
      frameHeight / imageSize.height,
    ) * nextZoom
    const renderedWidth = imageSize.width * scale
    const renderedHeight = imageSize.height * scale
    const maxX = Math.max(0, (renderedWidth - frameWidth) / 2)
    const maxY = Math.max(0, (renderedHeight - frameHeight) / 2)

    return {
      x: Math.min(maxX, Math.max(-maxX, next.x)),
      y: Math.min(maxY, Math.max(-maxY, next.y)),
    }
  }

  useEffect(() => {
    setPan((current) => clampPan(current))
    // Re-clamp when the source image changes size.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [imageSize.width, imageSize.height])

  const complete = async () => {
    const frame = frameRef.current
    const image = imageRef.current
    if (!frame || !image || imageSize.width === 0 || imageSize.height === 0) return

    setBusy(true)
    try {
      const frameWidth = frame.clientWidth
      const frameHeight = frame.clientHeight
      const scale = Math.max(
        frameWidth / imageSize.width,
        frameHeight / imageSize.height,
      ) * zoom
      const renderedWidth = imageSize.width * scale
      const renderedHeight = imageSize.height * scale
      const left = (frameWidth - renderedWidth) / 2 + pan.x
      const top = (frameHeight - renderedHeight) / 2 + pan.y
      const sourceX = Math.max(0, -left / scale)
      const sourceY = Math.max(0, -top / scale)
      const sourceWidth = Math.min(imageSize.width - sourceX, frameWidth / scale)
      const sourceHeight = Math.min(imageSize.height - sourceY, frameHeight / scale)

      const canvas = document.createElement('canvas')
      canvas.width = config.width
      canvas.height = config.height
      const ctx = canvas.getContext('2d')
      if (!ctx) throw new Error(t({ id: 'eunha.profile.could_not_prepare_image_crop', defaultMessage: 'Could not prepare image crop' }))
      ctx.imageSmoothingQuality = 'high'
      ctx.drawImage(
        image,
        sourceX,
        sourceY,
        sourceWidth,
        sourceHeight,
        0,
        0,
        config.width,
        config.height,
      )
      const blob = await new Promise<Blob>((resolve, reject) => {
        canvas.toBlob(
          (result) => {
            if (result) resolve(result)
            else reject(new Error(t({ id: 'eunha.profile.could_not_export_image_crop', defaultMessage: 'Could not export image crop' })))
          },
          draft.file.type || 'image/png',
          0.92,
        )
      })
      onComplete(new File([blob], draft.file.name, { type: blob.type }))
    } catch {
      setBusy(false)
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 px-3 py-8">
      <div className="bg-background w-full max-w-xl border shadow-lg">
        <div className="flex items-center justify-between border-b px-4 py-3">
          <div className="font-semibold">
            {draft.kind === 'avatar' ? t({ id: 'eunha.profile.crop_avatar', defaultMessage: 'Crop avatar' }) : t({ id: 'eunha.profile.crop_header', defaultMessage: 'Crop header' })}
          </div>
          <Button variant="ghost" size="sm" onClick={onCancel} disabled={busy}>
            {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
          </Button>
        </div>
        <div className="space-y-4 p-4">
          <div
            ref={frameRef}
            className="bg-muted relative w-full touch-none overflow-hidden border"
            style={{ aspectRatio: String(config.aspect) }}
            onPointerDown={(event) => {
              event.currentTarget.setPointerCapture(event.pointerId)
              dragRef.current = {
                pointerId: event.pointerId,
                x: event.clientX,
                y: event.clientY,
              }
            }}
            onPointerMove={(event) => {
              const drag = dragRef.current
              if (!drag || drag.pointerId !== event.pointerId) return
              const dx = event.clientX - drag.x
              const dy = event.clientY - drag.y
              dragRef.current = { ...drag, x: event.clientX, y: event.clientY }
              setPan((current) => clampPan({ x: current.x + dx, y: current.y + dy }))
            }}
            onPointerUp={(event) => {
              if (dragRef.current?.pointerId === event.pointerId) dragRef.current = null
            }}
            onPointerCancel={() => {
              dragRef.current = null
            }}
          >
            <img
              ref={imageRef}
              src={draft.url}
              alt=""
              draggable={false}
              className="absolute top-1/2 left-1/2 max-w-none select-none"
              style={{
                width: imageSize.width
                  ? `${imageSize.width * Math.max(
                      (frameRef.current?.clientWidth ?? 1) / imageSize.width,
                      (frameRef.current?.clientHeight ?? 1) / imageSize.height,
                    ) * zoom}px`
                  : undefined,
                transform: `translate(calc(-50% + ${pan.x}px), calc(-50% + ${pan.y}px))`,
              }}
              onLoad={(event) => {
                setImageSize({
                  width: event.currentTarget.naturalWidth,
                  height: event.currentTarget.naturalHeight,
                })
              }}
            />
          </div>
          <label className="grid gap-2 text-sm">
            <span className="text-muted-foreground">{t({ id: 'account_edit.upload_modal.step_crop.zoom', defaultMessage: 'Zoom' })}</span>
            <input
              type="range"
              min="1"
              max="3"
              step="0.01"
              value={zoom}
              onChange={(event) => {
                const nextZoom = event.currentTarget.valueAsNumber
                setZoom(nextZoom)
                setPan((current) => clampPan(current, nextZoom))
              }}
            />
          </label>
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={onCancel} disabled={busy}>
              {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
            </Button>
            <Button onClick={() => void complete()} disabled={busy || imageSize.width === 0}>
              {t({ id: 'lists.save', defaultMessage: 'Save' })}
            </Button>
          </div>
        </div>
      </div>
    </div>
  )
}

/**
 * `email_subscriptions` from the account entity, which masto.js does not
 * model: present and true while the account offers subscriptions by email.
 */
function offersEmailSubscriptions(account: mastodon.v1.Account): boolean {
  const extra = account as unknown as {
    emailSubscriptions?: boolean
    email_subscriptions?: boolean
  }
  return extra.emailSubscriptions === true || extra.email_subscriptions === true
}

function PrivateNote({ accountId, token, note, onSaved }: {
  accountId: string
  token: string
  note: string
  onSaved: (relationship: mastodon.v1.Relationship) => void
}) {
  const [draft, setDraft] = useState<string>()
  const [saving, setSaving] = useState(false)

  const save = async () => {
    if (draft === undefined) return
    setSaving(true)
    try {
      onSaved(await setAccountNote(accountId, token, draft))
      setDraft(undefined)
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="mt-3 space-y-2 rounded-md border p-3 text-sm">
      <h2 className="font-medium">{t({ id: 'eunha.profile.private_note', defaultMessage: 'Private note' })}</h2>
      {draft === undefined ? (
        <>
          {note && <p className="break-words whitespace-pre-wrap">{note}</p>}
          <Button variant="outline" size="sm" onClick={() => setDraft(note)}>
            {note ? t({ id: 'eunha.profile.edit_note', defaultMessage: 'Edit note' }) : t({ id: 'mastodon.admin.reports.notes.create', defaultMessage: 'Add note' })}
          </Button>
        </>
      ) : (
        <>
          <Textarea aria-label={t({ id: 'eunha.profile.private_note', defaultMessage: 'Private note' })} value={draft} maxLength={2000} disabled={saving}
            onChange={(event) => setDraft(event.target.value)} />
          <div className="flex gap-2">
            <Button size="sm" disabled={saving} onClick={save}>{t({ id: 'eunha.profile.save_note', defaultMessage: 'Save note' })}</Button>
            <Button size="sm" variant="ghost" disabled={saving} onClick={() => setDraft(undefined)}>{t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}</Button>
          </div>
        </>
      )}
    </section>
  )
}

export default function Profile() {
  const { acct = '', tagged } = useParams()
  const handle = acct.replace(/^@/, '')
  const token = getToken()
  const { pathname } = useLocation()
  const view = profileView(pathname)
  // `/@acct/with_replies` is the activity tab with replies, whatever the
  // switch says.
  const repliesForced = /\/with_replies\/?$/.test(pathname)
  const [filters, setFilters] = useActivityFilters()
  const replies = repliesForced || filters.replies
  const [featuredTags, setFeaturedTags] = useState<mastodon.v1.FeaturedTag[]>([])

  const [account, setAccount] = useState<mastodon.v1.Account | null>(null)
  const [rel, setRel] = useState<mastodon.v1.Relationship | null>(null)
  const [selfId, setSelfId] = useState<string | null>(null)
  const [sourceNote, setSourceNote] = useState('')
  const [sourceDisplayName, setSourceDisplayName] = useState('')
  const [sourceFields, setSourceFields] = useState<EditField[]>([])
  const [editingProfile, setEditingProfile] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [relationshipBusy, setRelationshipBusy] = useState(false)
  // Only the follow button spins; the other relationship actions are in a menu.
  const [followBusy, setFollowBusy] = useState(false)
  const followSettling = usePendingFollow(rel, token, setRel, account?.locked)
  const [confirmingBlock, setConfirmingBlock] = useState(false)
  const [reporting, setReporting] = useState(false)
  const { openCompose } = useComposeModal()
  const [pinned, setPinned] = useState<mastodon.v1.Status[] | null>(null)
  const [showAllPinned, setShowAllPinned] = useState(false)
  const [imageBusy, setImageBusy] = useState<'avatar' | 'header' | null>(null)
  const [imageError, setImageError] = useState<string | null>(null)
  const [cropDraft, setCropDraft] = useState<CropDraft | null>(null)
  const avatarInputRef = useRef<HTMLInputElement>(null)
  const headerInputRef = useRef<HTMLInputElement>(null)
  const avatar = useAnimatedImage(account?.avatar, account?.avatarStatic)
  const headerSrc = useAnimatedHeader(account?.header, account?.headerStatic)

  // Upstream's activity tab: `exclude_replies` and `exclude_reblogs` from the
  // switches, and `tagged` from a featured hashtag. The other tabs page their
  // own statuses.
  const feed = useInfiniteFeed<mastodon.v1.Status>(
    (maxId) =>
      account && view === 'activity'
        ? getAccountStatuses(account.id, token ?? undefined, maxId, {
            excludeReplies: !replies,
            excludeReblogs: !filters.boosts,
            tagged,
          })
        : Promise.resolve([]),
    [account?.id, token, view, replies, filters.boosts, tagged],
    `profile:${handle}`,
  )
  const statuses = withoutMessages(feed.items)

  // Pins come back in one response (five at most for a local account, however
  // many a remote one features) and are ordered by when they were pinned, so
  // they are fetched apart from the timeline feed rather than paged with it.
  // `loadPinned` is also what a card calls after pinning, so the section above
  // it agrees with the menu that changed it.
  const loadPinned = useCallback(() => {
    if (!account) return
    getPinnedStatuses(account.id, token ?? undefined, tagged)
      .then(setPinned)
      .catch(() => setPinned([]))
  }, [account, token, tagged])

  useEffect(() => {
    setFeaturedTags([])
    if (!account) return
    getAccountFeaturedTags(account.id, token ?? undefined)
      .then(setFeaturedTags)
      .catch(() => {})
  }, [account, token])

  useEffect(() => {
    setPinned(null)
    loadPinned()
  }, [loadPinned])

  // Upstream's `usePinnedStatusIds`: only the newest pin is shown until the
  // reader asks for the rest, so a profile with many pins does not push its
  // recent posts out of sight. Another profile starts collapsed again.
  useEffect(() => setShowAllPinned(false), [account?.id])
  const shownPinned =
    pinned && !showAllPinned ? pinned.slice(0, 1) : pinned

  // Mastodon's profile page carries `<meta name="robots" content="noindex,
  // noarchive">` for an account whose owner asked search engines to stay
  // away (`noindex`).
  useEffect(() => {
    if (!account?.noindex) return
    const meta = document.createElement('meta')
    meta.name = 'robots'
    meta.content = 'noindex, noarchive'
    document.head.appendChild(meta)
    return () => meta.remove()
  }, [account?.noindex])

  useEffect(() => {
    setAccount(null)
    setRel(null)
    setError(null)
    setImageError(null)
    lookupAccount(handle, token ?? undefined)
      .then((acc) => {
        setAccount(acc)
        if (token) {
          getRelationship(acc.id, token).then((r) => setRel(r ?? null)).catch(() => {})
        }
      })
      .catch((e) => setError(String(e)))
    if (token) {
      getCurrentAccount(token)
        .then((me) => {
          setSelfId(me.id)
          setSourceNote(me.source?.note ?? '')
          setSourceDisplayName(me.displayName ?? '')
          setSourceFields(
            (me.source?.fields ?? []).map((f) => ({
              name: f.name,
              value: f.value,
            })),
          )
        })
        .catch(() => {})
    }
  }, [handle, token])

  const toggleFollow = async () => {
    if (!account || !token || !rel || relationshipBusy) return
    setRelationshipBusy(true)
    setFollowBusy(true)
    try {
      // A pending request is undone as a follow is: `unfollow` withdraws it,
      // as upstream's `UnfollowService` does. Following again would only
      // send the request once more.
      setRel(await setFollow(account.id, token, !(rel.following || rel.requested)))
    } finally {
      setRelationshipBusy(false)
      setFollowBusy(false)
    }
  }

  const toggleReblogs = async () => {
    if (!account || !token || !rel?.following || relationshipBusy) return
    setRelationshipBusy(true)
    try {
      setRel(
        await setFollow(account.id, token, true, {
          reblogs: !rel.showingReblogs,
        }),
      )
    } finally {
      setRelationshipBusy(false)
    }
  }

  // Muting is about the timeline, not about the person: eunha keeps letting a
  // muted account mention you and react to your posts (the
  // `mute-does-not-silence-replies-to-me` divergence), so the copy here says
  // what the mute will and will not do rather than leaving it to be discovered.
  const toggleMute = async () => {
    if (!account || !token || !rel || relationshipBusy) return
    const muting = !rel.muting
    setRelationshipBusy(true)
    try {
      setRel(await setMute(account.id, token, muting))
      toast.success(
        muting
          ? t({ id: 'eunha.profile.muted_their_posts_are_hidden_from', defaultMessage: 'Muted @{acct}. Their posts are hidden from your timelines; they can still mention you and react to your posts.' }, { acct: account.acct })
          : t({ id: 'eunha.profile.unmuted', defaultMessage: 'Unmuted @{acct}.' }, { acct: account.acct }),
      )
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setRelationshipBusy(false)
    }
  }

  // Unlike a mute, a block is not just a view setting: the server severs any
  // follow in either direction and drops pending requests, and unblocking does
  // not put them back. So blocking asks first; unblocking does not need to.
  const toggleBlock = async () => {
    if (!account || !token || !rel || relationshipBusy) return
    const blocking = !rel.blocking
    setRelationshipBusy(true)
    try {
      setRel(await setBlock(account.id, token, blocking))
      toast.success(
        blocking
          ? t({ id: 'eunha.profile.blocked_they_cannot_follow_you_or', defaultMessage: 'Blocked @{acct}. They cannot follow you or see your posts, and any follow between you is undone.' }, { acct: account.acct })
          : t({ id: 'eunha.profile.unblocked', defaultMessage: 'Unblocked @{acct}.' }, { acct: account.acct }),
      )
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setRelationshipBusy(false)
      setConfirmingBlock(false)
    }
  }

  const isSelf = account != null && account.id === selfId
  // A remote account's own server has the full profile; eunha shows only
  // what has federated here.
  const remoteUrl = account?.acct.includes('@') ? account.url : null
  const signedInOther = !!token && !!rel && !isSelf
  const actionsMenu = account && (signedInOther || remoteUrl) && (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={
          <Button
            size="sm"
            variant="outline"
            aria-label={t({ id: 'eunha.profile.more_actions_for', defaultMessage: 'More actions for @{acct}' }, { acct: account.acct })}
          >
            <MoreHorizontal />
          </Button>
        }
      />
      <DropdownMenuContent align="end" className="w-auto">
        {signedInOther && rel && (
          <>
            <DropdownMenuItem
              onClick={() => openCompose({ messageTo: account })}
            >
              <MessageCircle /> {t({ id: 'account.menu.message', defaultMessage: 'Message' })}
            </DropdownMenuItem>
            <DropdownMenuItem
              onClick={toggleMute}
              disabled={relationshipBusy}
            >
              <VolumeX /> {rel.muting ? t({ id: 'account.unmute_short', defaultMessage: 'Unmute' }) : t({ id: 'account.mute_short', defaultMessage: 'Mute' })}
            </DropdownMenuItem>
            <DropdownMenuItem
              variant={rel.blocking ? undefined : 'destructive'}
              onClick={() =>
                rel.blocking ? toggleBlock() : setConfirmingBlock(true)
              }
              disabled={relationshipBusy}
            >
              <Ban /> {rel.blocking ? t({ id: 'account.unblock_short', defaultMessage: 'Unblock' }) : t({ id: 'account.block_short', defaultMessage: 'Block' })}
            </DropdownMenuItem>
            <DropdownMenuItem
              variant="destructive"
              onClick={() => setReporting(true)}
            >
              <Flag /> {t({ id: 'account.menu.report', defaultMessage: 'Report account' })}
            </DropdownMenuItem>
          </>
        )}
        {remoteUrl && (
          <DropdownMenuItem
            className="no-underline"
            render={
              <a
                href={remoteUrl}
                target="_blank"
                rel="noopener noreferrer"
              />
            }
          >
            <ExternalLink /> {t({ id: 'account.open_original_page', defaultMessage: 'Open original page' })}
          </DropdownMenuItem>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  )

  const applyUpdatedAccount = (updated: mastodon.v1.Account) => {
    setAccount((current) =>
      current
        ? {
            ...current,
            avatar: updated.avatar,
            avatarStatic: updated.avatarStatic,
            header: updated.header,
            headerStatic: updated.headerStatic,
          }
        : updated,
    )
  }

  const updateProfileImage = async (
    kind: 'avatar' | 'header',
    file: File | null,
  ) => {
    if (!token || !isSelf || imageBusy) return
    setImageBusy(kind)
    setImageError(null)
    try {
      const updated = file
        ? await updateAccountImages(token, { [kind]: file })
        : kind === 'avatar'
          ? await deleteProfileAvatar(token)
          : await deleteProfileHeader(token)
      applyUpdatedAccount(updated)
    } catch (e) {
      setImageError(e instanceof Error ? e.message : String(e))
    } finally {
      setImageBusy(null)
    }
  }

  const onProfileSaved = (updated: mastodon.v1.AccountCredentials) => {
    setAccount((current) =>
      current
        ? {
            ...current,
            displayName: updated.displayName,
            note: updated.note,
            fields: updated.fields,
            emojis: updated.emojis,
          }
        : updated,
    )
    setSourceNote(updated.source?.note ?? '')
    setSourceDisplayName(updated.displayName ?? '')
    setSourceFields(
      (updated.source?.fields ?? []).map((f) => ({
        name: f.name,
        value: f.value,
      })),
    )
    setEditingProfile(false)
  }

  const onImageSelected =
    (kind: 'avatar' | 'header') => (event: React.ChangeEvent<HTMLInputElement>) => {
      const file = event.currentTarget.files?.[0] ?? null
      event.currentTarget.value = ''
      if (!file) return
      if (file.type === 'image/gif') {
        void updateProfileImage(kind, file)
        return
      }
      setCropDraft({ kind, file, url: URL.createObjectURL(file) })
    }

  const closeCrop = () => {
    if (cropDraft) URL.revokeObjectURL(cropDraft.url)
    setCropDraft(null)
  }

  return (
    <div className="page-frame">
      <TopBar />
      {cropDraft && (
        <ProfileImageCropModal
          draft={cropDraft}
          onCancel={closeCrop}
          onComplete={(file) => {
            const kind = cropDraft.kind
            closeCrop()
            void updateProfileImage(kind, file)
          }}
        />
      )}
      {editingProfile && token && (
        <ProfileEditModal
          token={token}
          initialDisplayName={sourceDisplayName}
          initialNote={sourceNote}
          initialFields={sourceFields}
          onCancel={() => setEditingProfile(false)}
          onSaved={onProfileSaved}
        />
      )}
      {error && <p className="text-destructive text-sm">{error}</p>}
      {/* A suspended (or deleted) account is still served, with every profile
          field blanked and `suspended: true` — show that rather than the empty
          shell of a profile it would otherwise render. */}
      {!account && !error && <ProfileSkeleton />}
      {account?.suspended && (
        <div className="rounded-lg border p-6 text-center">
          <h1 className="font-semibold">{t({ id: 'empty_column.account_suspended', defaultMessage: 'Account suspended' })}</h1>
          <p className="text-muted-foreground mt-1 text-sm">
            {t({ id: 'eunha.profile.no_longer_available', defaultMessage: 'The profile of @{acct} is no longer available.' }, { acct: account.acct })}
          </p>
        </div>
      )}
      {account && !account.suspended && (
        <>
          {isSelf && (
            <>
              <input
                ref={avatarInputRef}
                type="file"
                accept="image/*"
                className="hidden"
                onChange={onImageSelected('avatar')}
              />
              <input
                ref={headerInputRef}
                type="file"
                accept="image/*"
                className="hidden"
                onChange={onImageSelected('header')}
              />
            </>
          )}
          {/* Mastodon's account header animates every emoji in it — the
              name's, the bio's, the fields' — while the pointer is over it. */}
          <AnimateEmoji>
          {hasCustomHeader(account.header) && (
            <div className="relative">
              <img
                src={headerSrc}
                alt=""
                className="h-32 w-full rounded-xl object-cover"
              />
              {isSelf && (
                <div className="absolute top-2 right-2 flex gap-1">
                  <Button
                    size="icon"
                    variant="outline"
                    className="bg-background/90 size-8"
                    aria-label={t({ id: 'eunha.profile.upload_header_image', defaultMessage: 'Upload header image' })}
                    disabled={imageBusy !== null}
                    onClick={() => headerInputRef.current?.click()}
                  >
                    <ImageUp />
                  </Button>
                  <Button
                    size="icon"
                    variant="outline"
                    className="bg-background/90 size-8"
                    aria-label={t({ id: 'eunha.profile.remove_header_image', defaultMessage: 'Remove header image' })}
                    disabled={imageBusy !== null}
                    onClick={() => void updateProfileImage('header', null)}
                  >
                    <Trash2 />
                  </Button>
                </div>
              )}
            </div>
          )}
          {!hasCustomHeader(account.header) && isSelf && (
            <div className="mb-2 flex justify-end">
              <Button
                size="sm"
                variant="outline"
                disabled={imageBusy !== null}
                onClick={() => headerInputRef.current?.click()}
              >
                <ImageUp /> {t({ id: 'mastodon.admin.accounts.header', defaultMessage: 'Header' })}
              </Button>
            </div>
          )}
          <div className="mt-3 flex items-start gap-3">
            <div className="flex flex-col items-center gap-1">
              <Avatar className="size-16" {...avatar.hover}>
                <AvatarImage src={avatar.src} alt="" />
                <AvatarFallback>
                  {(account.displayName || account.username).slice(0, 1).toUpperCase()}
                </AvatarFallback>
              </Avatar>
              {isSelf && (
                <div className="flex gap-1">
                  <Button
                    size="icon"
                    variant="ghost"
                    className="size-7"
                    aria-label={t({ id: 'eunha.profile.upload_avatar_image', defaultMessage: 'Upload avatar image' })}
                    disabled={imageBusy !== null}
                    onClick={() => avatarInputRef.current?.click()}
                  >
                    <ImageUp />
                  </Button>
                  <Button
                    size="icon"
                    variant="ghost"
                    className="size-7"
                    aria-label={t({ id: 'eunha.profile.remove_avatar_image', defaultMessage: 'Remove avatar image' })}
                    disabled={imageBusy !== null}
                    onClick={() => void updateProfileImage('avatar', null)}
                  >
                    <Trash2 />
                  </Button>
                </div>
              )}
            </div>
            <div className="flex-1">
              <div className="text-xl font-bold">
                <DisplayName account={account} />
              </div>
              <div className="text-muted-foreground">@{account.acct}</div>
            </div>
            {isSelf && (
              <div className="flex flex-col items-end gap-2">
                <Button size="sm" variant="outline" onClick={() => setEditingProfile(true)}>
                  <Pencil /> {t({ id: 'account.edit_profile', defaultMessage: 'Edit profile' })}
                </Button>
                <Button
                  render={<Link to="/follow-requests" className="no-underline" />}
                  size="sm"
                  variant="outline"
                >
                  <UserPlus /> {t({ id: 'column.follow_requests', defaultMessage: 'Follow requests' })}
                </Button>
              </div>
            )}
            {!signedInOther && actionsMenu}
            {signedInOther && rel && (
              <div className="flex flex-col items-end gap-2">
                <div className="flex items-center gap-2">
                  <Button
                    size="sm"
                    variant={rel.following || rel.requested ? 'outline' : 'default'}
                    onClick={toggleFollow}
                    disabled={relationshipBusy}
                    aria-busy={followBusy || followSettling}
                    title={rel.requested && !rel.following ? t({ id: 'eunha.profile.withdraw_follow_request', defaultMessage: 'Withdraw follow request' }) : undefined}
                  >
                    <FollowButtonIcon busy={followBusy || followSettling} requested={rel.requested && !rel.following} />
                    {rel.following ? t({ id: 'account.following', defaultMessage: 'Following' }) : rel.requested ? t({ id: 'eunha.profile.requested', defaultMessage: 'Requested' }) : t({ id: 'account.follow', defaultMessage: 'Follow' })}
                  </Button>
                  {actionsMenu}
                </div>
                {rel.muting && (
                  <p className="text-muted-foreground max-w-64 text-right text-xs">
                    {t({ id: 'eunha.profile.muted_their_posts_are_hidden_from_2', defaultMessage: 'Muted — their posts are hidden from your timelines. They can still mention you and react to your posts.' })}
                  </p>
                )}
                {rel.blocking && (
                  <p className="text-muted-foreground max-w-64 text-right text-xs">
                    {t({ id: 'eunha.profile.blocked_they_cannot_follow_you_or_2', defaultMessage: 'Blocked — they cannot follow you or see your posts, and you will not see theirs.' })}
                  </p>
                )}
                {rel.blockedBy && !rel.blocking && (
                  <p className="text-muted-foreground max-w-64 text-right text-xs">
                    {t({ id: 'eunha.profile.this_account_has_blocked_you', defaultMessage: 'This account has blocked you.' })}
                  </p>
                )}
                {rel.following && (
                  <Label className="text-xs font-normal">
                    <Switch
                      size="sm"
                      checked={rel.showingReblogs}
                      onCheckedChange={toggleReblogs}
                      disabled={relationshipBusy}
                    />
                    {t({ id: 'account.filters.boosts_toggle', defaultMessage: 'Show boosts' })}
                  </Label>
                )}
              </div>
            )}
          </div>
          {token && rel && !isSelf && (
            <PrivateNote key={account.id} accountId={account.id} token={token} note={rel.note ?? ''} onSaved={setRel} />
          )}
          {imageError && (
            <p className="text-destructive mt-2 text-sm">{imageError}</p>
          )}
          {account.note && (
            <EmojiHtml
              className="rich-text mt-3 text-sm [&_a]:font-medium [&_a]:text-primary [&_a]:underline [&_a]:decoration-primary [&_a]:decoration-2 [&_a]:underline-offset-2"
              html={account.note}
              emojis={account.emojis}
            />
          )}
          {account.fields.length > 0 && (
            <dl className="mt-3 divide-y rounded-md border text-sm">
              {account.fields.map((field, index) => (
                <div
                  key={index}
                  className={`grid grid-cols-[minmax(0,1fr)_minmax(0,2fr)] gap-2 px-3 py-2 ${
                    field.verifiedAt ? 'bg-emerald-500/10' : ''
                  }`}
                >
                  {/* A field's name is plain text, a remote account's as it
                      arrived: Mastodon escapes it, and so does React. */}
                  <dt className="text-muted-foreground truncate font-medium">
                    <EmojiText text={field.name} emojis={account.emojis} />
                  </dt>
                  <dd className="flex min-w-0 items-center gap-1">
                    <EmojiHtml
                      as="span"
                      className="[&_a]:text-primary min-w-0 truncate [&_a]:underline"
                      html={field.value}
                      emojis={account.emojis}
                    />
                    {field.verifiedAt && (
                      <CheckCircle2
                        className="size-4 shrink-0 text-emerald-600"
                        aria-label={t({ id: 'eunha.profile.verified_on', defaultMessage: 'Verified {date}' }, { date: new Date(field.verifiedAt).toLocaleDateString(currentLocale()) })}
                      />
                    )}
                  </dd>
                </div>
              ))}
            </dl>
          )}
          </AnimateEmoji>
          <div className="text-muted-foreground mt-3 mb-4 flex gap-4 text-sm">
            <span>
              <FormattedMessage
                id='account.statuses_counter'
                defaultMessage='{count, plural, one {{counter} post} other {{counter} posts}}'
                values={{ count: account.statusesCount, counter: <b className="text-foreground">{account.statusesCount}</b> }}
              />
            </span>
            <Link
              to={`/@${account.acct}/following`}
              className="no-underline hover:underline"
            >
              <FormattedMessage
                id='account.following_counter'
                defaultMessage='{count, plural, one {{counter} following} other {{counter} following}}'
                values={{ count: account.followingCount, counter: <b className="text-foreground">{account.followingCount}</b> }}
              />
            </Link>
            <Link
              to={`/@${account.acct}/followers`}
              className="no-underline hover:underline"
            >
              <FormattedMessage
                id='account.followers_counter'
                defaultMessage='{count, plural, one {{counter} follower} other {{counter} followers}}'
                values={{ count: account.followersCount, counter: <b className="text-foreground">{account.followersCount}</b> }}
              />
            </Link>
          </div>
          {/* Mastodon shows it to visitors only: `!me && account.email_subscriptions`. */}
          {!token && offersEmailSubscriptions(account) && (
            <EmailSubscriptionForm
              accountId={account.id}
              name={account.displayName || account.username}
            />
          )}

          {/* Upstream sends a visitor of a hidden tab back to the profile. */}
          {!offeredViews(account).includes(view) && (
            <Navigate to={`/@${account.acct}`} replace />
          )}
          <ProfileTabs account={account} />
          {view === 'media' && <MediaGrid account={account} token={token ?? undefined} />}
          {view === 'featured' && (
            <FeaturedPane account={account} token={token ?? undefined} isSelf={isSelf} />
          )}
          {view === 'activity' && (
          <div className="space-y-2">
            <ActivityFilterBar
              account={account}
              filters={filters}
              onChange={setFilters}
              repliesForced={repliesForced}
              featuredTags={featuredTags}
              tagged={tagged}
            />
            {!!pinned?.length && (
              <section className="space-y-1">
                <h2 className="text-muted-foreground flex items-center gap-1.5 text-sm font-semibold">
                  <Pin className="size-3.5" /> {t({ id: 'account.timeline.pinned', defaultMessage: 'Pinned' })}
                </h2>
                <TimelineStack>
                  {shownPinned?.map((s) => (
                    <StatusCard
                      key={s.id}
                      status={s}
                      token={token ?? ''}
                      filterContext="account"
                      onPinChange={loadPinned}
                    />
                  ))}
                </TimelineStack>
                {pinned.length > 1 && !showAllPinned && (
                  <Button
                    variant="outline"
                    size="sm"
                    className="w-full"
                    onClick={() => setShowAllPinned(true)}
                  >
                    <Pin /> {t({ id: 'account.timeline.pinned.view_all', defaultMessage: 'View all pinned posts' })}
                  </Button>
                )}
              </section>
            )}
            {statuses === null && !error && (
              <FeedSkeleton />
            )}
            {!!statuses?.length && (
              <TimelineStack>
                {statuses.map((s) => (
                  <StatusCard
                    key={s.id}
                    status={s.reblog ?? s}
                    token={token ?? ''}
                    boostedBy={s.reblog ? s.account : undefined}
                    filterContext="account"
                    onPinChange={loadPinned}
                  />
                ))}
              </TimelineStack>
            )}
            {statuses?.length === 0 && (
              <p className="text-muted-foreground text-sm">{t({ id: 'eunha.profile.no_posts_yet', defaultMessage: 'No posts yet.' })}</p>
            )}
            <InfiniteScroll
              onLoadMore={feed.loadMore}
              loading={feed.loadingMore}
              done={feed.done}
              hasItems={!!statuses?.length}
            />
          </div>
          )}
        </>
      )}
      {account && token && !isSelf && (
        <ReportDialog
          account={account}
          open={reporting}
          onOpenChange={setReporting}
          token={token}
        />
      )}
      <AlertDialog open={confirmingBlock} onOpenChange={setConfirmingBlock}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t({ id: 'eunha.profile.block_title', defaultMessage: 'Block @{acct}?' }, { acct: account?.acct })}</AlertDialogTitle>
            <AlertDialogDescription>
              {t({ id: 'eunha.profile.they_will_not_be_able_to', defaultMessage: 'They will not be able to follow you or see your posts, and you will not see theirs. Any follow between you is undone now, and unblocking later does not restore it.' })}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={relationshipBusy}>{t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}</AlertDialogCancel>
            <AlertDialogAction
              variant="destructive"
              disabled={relationshipBusy}
              onClick={toggleBlock}
            >
              {relationshipBusy ? t({ id: 'eunha.common.blocking', defaultMessage: 'Blocking…' }) : t({ id: 'account.block_short', defaultMessage: 'Block' })}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  )
}
