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
          aria-label={`Category for ${emoji.shortcode}`}
          placeholder="No category"
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
        Enabled
      </Label>
      <Label className="text-sm font-normal">
        <Switch
          size="sm"
          disabled={busy}
          checked={emoji.visible_in_picker}
          onCheckedChange={(on) => void save({ visible_in_picker: on })}
        />
        In picker
      </Label>
      <ConfirmButton
        size="xs"
        title={`Delete :${emoji.shortcode}:?`}
        description="Posts that use it show the shortcode as text instead."
        confirmLabel="Delete"
        onConfirm={async () => {
          await deleteCustomEmoji(token, emoji.id)
          onDeleted(emoji.id)
          toast.success(`Deleted :${emoji.shortcode}:.`)
        }}
      >
        Delete
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
      toast.error('Shortcodes are at least two letters, digits or underscores.')
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
      toast.success(`Added :${emoji.shortcode}:.`)
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
    <AdminLayout title="Custom emoji" permission="manage_custom_emojis">
      <form onSubmit={upload} className="mb-4 space-y-2 rounded-lg border p-3">
        <h2 className="text-sm font-semibold">Upload</h2>
        <div className="grid gap-2 sm:grid-cols-2">
          <Input
            aria-label="Shortcode"
            placeholder="shortcode"
            value={shortcode}
            onChange={(e) => setShortcode(e.target.value.replace(/:/g, ''))}
          />
          <Input
            aria-label="Category"
            placeholder="Category (optional)"
            value={category}
            onChange={(e) => setCategory(e.target.value)}
          />
        </div>
        <Input
          key={fileKey}
          type="file"
          aria-label="Image"
          accept="image/png,image/gif,image/webp"
          onChange={(e) => setFile(e.target.files?.[0] ?? null)}
        />
        <p className="text-muted-foreground text-xs">PNG, GIF or WebP, up to 256 KB.</p>
        <Button type="submit" disabled={uploading || !file || !shortcode}>
          {uploading ? 'Uploading…' : 'Upload'}
        </Button>
      </form>

      <Input
        aria-label="Filter emoji"
        placeholder="Filter by shortcode or category"
        className="mb-3"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
      />
      <AdminError error={error} />
      {emojis === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
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
          {filter ? 'No emoji match.' : 'No custom emoji yet.'}
        </p>
      )}
    </AdminLayout>
  )
}
