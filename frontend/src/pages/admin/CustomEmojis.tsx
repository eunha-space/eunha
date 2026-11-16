import { useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import {
  createCustomEmoji,
  deleteCustomEmoji,
  listCustomEmojis,
  updateCustomEmoji,
  type AdminCustomEmoji,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ConfirmButton } from '@/components/admin/admin-common.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

// Mastodon's `CustomEmoji::SHORTCODE_RE_FRAGMENT`, anchored.
const SHORTCODE = /^[a-zA-Z0-9_]{2,}$/

function EmojiRow({
  emoji,
  token,
  onSaved,
  onDeleted,
}: {
  emoji: AdminCustomEmoji
  token: string
  onSaved: (e: AdminCustomEmoji) => void
  onDeleted: (id: string) => void
}) {
  const [busy, setBusy] = useState(false)
  const [category, setCategory] = useState(emoji.category ?? '')

  const save = async (params: Parameters<typeof updateCustomEmoji>[2]) => {
    setBusy(true)
    try {
      onSaved(await updateCustomEmoji(token, emoji.id, params))
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="flex flex-wrap items-center gap-3 p-2.5">
      <img
        src={emoji.static_url || emoji.url}
        alt={`:${emoji.shortcode}:`}
        className="size-8 object-contain"
      />
      <div className="min-w-0 flex-1 space-y-1">
        <div className="truncate font-mono text-sm">:{emoji.shortcode}:</div>
        <Input
          aria-label={t({ id: 'eunha.admin.custom_emojis.category_for', defaultMessage: 'Category for {shortcode}' }, { shortcode: emoji.shortcode })}
          placeholder={t({ id: 'eunha.admin.custom_emojis.no_category', defaultMessage: 'No category' })}
          value={category}
          disabled={busy}
          className="h-7 max-w-48 text-xs"
          onChange={(e) => setCategory(e.target.value)}
          onBlur={() => {
            if (category.trim() !== (emoji.category ?? '')) {
              void save({ category: category.trim() || null })
            }
          }}
        />
      </div>
      <Label className="text-sm font-normal">
        <Switch
          size="sm"
          disabled={busy}
          checked={!emoji.disabled}
          onCheckedChange={(on) => void save({ disabled: !on })}
        />
        {t({ id: 'mastodon.admin.custom_emojis.enabled', defaultMessage: 'Enabled' })}
      </Label>
      <Label className="text-sm font-normal">
        <Switch
          size="sm"
          disabled={busy}
          checked={emoji.visible_in_picker}
          onCheckedChange={(on) => void save({ visible_in_picker: on })}
        />
        {t({ id: 'eunha.admin.custom_emojis.in_picker', defaultMessage: 'In picker' })}
      </Label>
      <ConfirmButton
        size="xs"
        title={t({ id: 'eunha.admin.custom_emojis.delete', defaultMessage: 'Delete :{shortcode}:?' }, { shortcode: emoji.shortcode })}
        description={t({ id: 'eunha.admin.custom_emojis.posts_that_use_it_show_the', defaultMessage: 'Posts that use it show the shortcode as text instead.' })}
        confirmLabel={t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
        onConfirm={async () => {
          await deleteCustomEmoji(token, emoji.id)
          onDeleted(emoji.id)
          toast.success(t({ id: 'eunha.admin.custom_emojis.deleted', defaultMessage: 'Deleted :{shortcode}:.' }, { shortcode: emoji.shortcode }))
        }}
      >
        {t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
      </ConfirmButton>
    </div>
  )
}

/**
 * The server's own custom emoji. Mastodon manages these only from its web UI;
 * eunha serves `/api/v1/admin/custom_emojis` so this page can, and it lists
 * local emoji only — copying a remote one is not something it offers.
 */
export default function CustomEmojis() {
  const token = getToken()
  const [emojis, setEmojis] = useState<AdminCustomEmoji[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [shortcode, setShortcode] = useState('')
  const [category, setCategory] = useState('')
  const [file, setFile] = useState<File | null>(null)
  const [fileKey, setFileKey] = useState(0)
  const [uploading, setUploading] = useState(false)
  const [filter, setFilter] = useState('')

  useEffect(() => {
    if (!token) return
    listCustomEmojis(token)
      .then(setEmojis)
      .catch((e) => setError(String(e)))
  }, [token])

  const upload = async (e: FormEvent) => {
    e.preventDefault()
    if (!token || !file) return
    if (!SHORTCODE.test(shortcode)) {
      toast.error(t({ id: 'eunha.admin.custom_emojis.shortcodes_are_at_least_two_letters', defaultMessage: 'Shortcodes are at least two letters, digits or underscores.' }))
      return
    }
    setUploading(true)
    try {
      const emoji = await createCustomEmoji(token, {
        shortcode,
        image: file,
        category: category.trim() || undefined,
      })
      setEmojis((list) => [emoji, ...(list ?? []).filter((x) => x.id !== emoji.id)])
      setShortcode('')
      setFile(null)
      setFileKey((k) => k + 1)
      toast.success(t({ id: 'eunha.admin.custom_emojis.added', defaultMessage: 'Added :{shortcode}:.' }, { shortcode: emoji.shortcode }))
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setUploading(false)
    }
  }

  const shown = emojis?.filter(
    (e) =>
      !filter ||
      e.shortcode.toLowerCase().includes(filter.toLowerCase()) ||
      (e.category ?? '').toLowerCase().includes(filter.toLowerCase()),
  )

  return (
    <AdminLayout title={t({ id: 'eunha.admin.custom_emojis.custom_emoji', defaultMessage: 'Custom emoji' })} permission="manage_custom_emojis">
      <form onSubmit={upload} className="mb-4 space-y-2 rounded-lg border p-3">
        <h2 className="text-sm font-semibold">{t({ id: 'mastodon.admin.custom_emojis.upload', defaultMessage: 'Upload' })}</h2>
        <div className="grid gap-2 sm:grid-cols-2">
          <Input
            aria-label={t({ id: 'mastodon.admin.custom_emojis.shortcode', defaultMessage: 'Shortcode' })}
            placeholder="shortcode"
            value={shortcode}
            onChange={(e) => setShortcode(e.target.value.replace(/:/g, ''))}
          />
          <Input
            aria-label={t({ id: 'mastodon.admin.reports.category', defaultMessage: 'Category' })}
            placeholder={t({ id: 'eunha.admin.custom_emojis.category_optional', defaultMessage: 'Category (optional)' })}
            value={category}
            onChange={(e) => setCategory(e.target.value)}
          />
        </div>
        <Input
          key={fileKey}
          type="file"
          aria-label={t({ id: 'eunha.admin.custom_emojis.image', defaultMessage: 'Image' })}
          accept="image/png,image/gif,image/webp"
          onChange={(e) => setFile(e.target.files?.[0] ?? null)}
        />
        <p className="text-muted-foreground text-xs">{t({ id: 'eunha.admin.custom_emojis.png_gif_or_webp_up_to', defaultMessage: 'PNG, GIF or WebP, up to 256 KB.' })}</p>
        <Button type="submit" disabled={uploading || !file || !shortcode}>
          {uploading ? t({ id: 'eunha.common.uploading', defaultMessage: 'Uploading…' }) : t({ id: 'mastodon.admin.custom_emojis.upload', defaultMessage: 'Upload' })}
        </Button>
      </form>

      <Input
        aria-label={t({ id: 'eunha.admin.custom_emojis.filter_emoji', defaultMessage: 'Filter emoji' })}
        placeholder={t({ id: 'eunha.admin.custom_emojis.filter_by_shortcode_or_category', defaultMessage: 'Filter by shortcode or category' })}
        className="mb-3"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
      />
      <AdminError error={error} />
      {emojis === null && !error && <RowsSkeleton action />}
      <div className={shown?.length ? 'divide-y rounded-lg border' : ''}>
        {token &&
          shown?.map((e) => (
            <EmojiRow
              key={e.id}
              emoji={e}
              token={token}
              onSaved={(updated) =>
                setEmojis((list) => list?.map((x) => (x.id === updated.id ? updated : x)) ?? null)
              }
              onDeleted={(id) => setEmojis((list) => list?.filter((x) => x.id !== id) ?? null)}
            />
          ))}
      </div>
      {shown?.length === 0 && (
        <p className="text-muted-foreground text-sm">
          {filter ? t({ id: 'eunha.admin.custom_emojis.no_emoji_match', defaultMessage: 'No emoji match.' }) : t({ id: 'eunha.admin.custom_emojis.no_custom_emoji_yet', defaultMessage: 'No custom emoji yet.' })}
        </p>
      )}
    </AdminLayout>
  )
}
