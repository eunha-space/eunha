import { useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import {
  createWarningPreset,
  deleteWarningPreset,
  listWarningPresets,
  updateWarningPreset,
  type WarningPreset,
  type WarningPresetCategory,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ChoiceSelect, ConfirmButton } from '@/components/admin/admin-common.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { Badge } from '@/components/ui/badge.tsx'

/** `admin.warning_presets.dropdown`. */
const CATEGORIES: Record<WarningPresetCategory, string> = {
  all_actions: 'All actions',
  none: 'Warning',
  disable: 'Freeze',
  sensitive: 'Sensitive',
  silence: 'Limit',
  suspend: 'Suspend',
}

function PresetForm({
  initial,
  submitLabel,
  onSubmit,
  onCancel,
}: {
  initial?: WarningPreset
  submitLabel: string
  onSubmit: (title: string, text: string, category: WarningPresetCategory) => Promise<void>
  onCancel?: () => void
}) {
  const [title, setTitle] = useState(initial?.title ?? '')
  const [text, setText] = useState(initial?.text ?? '')
  const [category, setCategory] = useState<WarningPresetCategory>(
    initial?.action_category ?? 'all_actions',
  )
  const [saving, setSaving] = useState(false)
  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setSaving(true)
    try {
      await onSubmit(title, text, category)
      if (!initial) {
        setTitle('')
        setText('')
        setCategory('all_actions')
      }
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }
  return (
    <form onSubmit={submit} className="space-y-2 rounded-lg border p-3">
      <div className="space-y-1">
        <Label htmlFor={`preset-title-${initial?.id ?? 'new'}`}>Title</Label>
        <Input
          id={`preset-title-${initial?.id ?? 'new'}`}
          value={title}
          placeholder="Optional"
          onChange={(e) => setTitle(e.target.value)}
        />
      </div>
      <div className="space-y-1">
        <Label htmlFor={`preset-text-${initial?.id ?? 'new'}`}>Preset text</Label>
        <Textarea
          id={`preset-text-${initial?.id ?? 'new'}`}
          value={text}
          rows={3}
          className="resize-y"
          onChange={(e) => setText(e.target.value)}
        />
      </div>
      <div className="space-y-1">
        <Label>Action</Label>
        <ChoiceSelect
          label="Action"
          value={category}
          items={CATEGORIES}
          onChange={setCategory}
        />
      </div>
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={saving || !text.trim()}>
          {submitLabel}
        </Button>
        {onCancel && (
          <Button type="button" size="sm" variant="outline" onClick={onCancel}>
            Cancel
          </Button>
        )}
      </div>
    </form>
  )
}

/**
 * Warning presets: Mastodon's `Admin::WarningPresetsController`, canned texts
 * the moderation form offers to start a warning from.
 */
export default function WarningPresets() {
  const token = getToken()
  const [presets, setPresets] = useState<WarningPreset[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [editing, setEditing] = useState<string | null>(null)

  const load = () => {
    if (!token) return
    listWarningPresets(token)
      .then((p) => {
        setPresets(p)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token])

  return (
    <AdminLayout title="Warning presets" permission="manage_settings">
      <div className="mb-4">
        <PresetForm
          submitLabel="Add new preset"
          onSubmit={async (title, text, action_category) => {
            await createWarningPreset(token ?? '', { title, text, action_category })
            toast.success('Preset added.')
            load()
          }}
        />
      </div>
      <AdminError error={error} />
      {presets === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {presets?.length === 0 && (
        <p className="text-muted-foreground text-sm">No warning presets have been defined yet.</p>
      )}
      <div className="space-y-2">
        {presets?.map((p) =>
          editing === p.id ? (
            <PresetForm
              key={p.id}
              initial={p}
              submitLabel="Save changes"
              onCancel={() => setEditing(null)}
              onSubmit={async (title, text, action_category) => {
                await updateWarningPreset(token ?? '', p.id, { title, text, action_category })
                toast.success('Preset saved.')
                setEditing(null)
                load()
              }}
            />
          ) : (
            <div key={p.id} className="flex items-start gap-2 rounded-lg border p-3">
              <div className="min-w-0 flex-1 space-y-1">
                <div className="flex items-center gap-2 text-sm font-medium">
                  {p.title || '(untitled)'}
                  <Badge variant="secondary">{CATEGORIES[p.action_category]}</Badge>
                </div>
                <p className="text-muted-foreground text-sm whitespace-pre-wrap">{p.text}</p>
              </div>
              <Button size="xs" variant="outline" onClick={() => setEditing(p.id)}>
                Edit
              </Button>
              <ConfirmButton
                size="xs"
                title="Delete this preset?"
                description="Warnings already sent with it are not changed."
                confirmLabel="Delete"
                onConfirm={async () => {
                  await deleteWarningPreset(token ?? '', p.id)
                  setPresets((list) => (list ?? []).filter((x) => x.id !== p.id))
                  toast.success('Preset deleted.')
                }}
              >
                Delete
              </ConfirmButton>
            </div>
          ),
        )}
      </div>
    </AdminLayout>
  )
}
