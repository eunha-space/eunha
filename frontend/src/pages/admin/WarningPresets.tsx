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
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

/** `admin.warning_presets.dropdown`. */
const CATEGORIES: Record<WarningPresetCategory, string> = {
  get all_actions() { return t({ id: 'eunha.admin.warning_presets.all_actions', defaultMessage: 'All actions' }) },
  get none() { return t({ id: 'mastodon.admin.instances.back_to_warning', defaultMessage: 'Warning' }) },
  get disable() { return t({ id: 'mastodon.admin.accounts.disable', defaultMessage: 'Freeze' }) },
  get sensitive() { return t({ id: 'collections.sensitive', defaultMessage: 'Sensitive' }) },
  get silence() { return t({ id: 'mastodon.admin.accounts.silence', defaultMessage: 'Limit' }) },
  get suspend() { return t({ id: 'mastodon.admin.accounts.perform_full_suspension', defaultMessage: 'Suspend' }) },
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
        <Label htmlFor={`preset-title-${initial?.id ?? 'new'}`}>{t({ id: 'mastodon.simple_form.labels.account_warning_preset.title', defaultMessage: 'Title' })}</Label>
        <Input
          id={`preset-title-${initial?.id ?? 'new'}`}
          value={title}
          placeholder={t({ id: 'eunha.admin.warning_presets.optional', defaultMessage: 'Optional' })}
          onChange={(e) => setTitle(e.target.value)}
        />
      </div>
      <div className="space-y-1">
        <Label htmlFor={`preset-text-${initial?.id ?? 'new'}`}>{t({ id: 'eunha.admin.warning_presets.preset_text', defaultMessage: 'Preset text' })}</Label>
        <Textarea
          id={`preset-text-${initial?.id ?? 'new'}`}
          value={text}
          rows={3}
          className="resize-y"
          onChange={(e) => setText(e.target.value)}
        />
      </div>
      <div className="space-y-1">
        <Label>{t({ id: 'mastodon.simple_form.labels.admin_account_action.type', defaultMessage: 'Action' })}</Label>
        <ChoiceSelect
          label={t({ id: 'mastodon.simple_form.labels.admin_account_action.type', defaultMessage: 'Action' })}
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
            {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
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
    <AdminLayout title={t({ id: 'eunha.admin.warning_presets.warning_presets', defaultMessage: 'Warning presets' })} permission="manage_settings">
      <div className="mb-4">
        <PresetForm
          submitLabel={t({ id: 'eunha.admin.warning_presets.add_new_preset', defaultMessage: 'Add new preset' })}
          onSubmit={async (title, text, action_category) => {
            await createWarningPreset(token ?? '', { title, text, action_category })
            toast.success(t({ id: 'eunha.admin.warning_presets.preset_added', defaultMessage: 'Preset added.' }))
            load()
          }}
        />
      </div>
      <AdminError error={error} />
      {presets === null && !error && <RowsSkeleton action />}
      {presets?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.warning_presets.no_warning_presets_have_been_defined', defaultMessage: 'No warning presets have been defined yet.' })}</p>
      )}
      <div className="space-y-2">
        {presets?.map((p) =>
          editing === p.id ? (
            <PresetForm
              key={p.id}
              initial={p}
              submitLabel={t({ id: 'subscribed_languages.save', defaultMessage: 'Save changes' })}
              onCancel={() => setEditing(null)}
              onSubmit={async (title, text, action_category) => {
                await updateWarningPreset(token ?? '', p.id, { title, text, action_category })
                toast.success(t({ id: 'eunha.admin.warning_presets.preset_saved', defaultMessage: 'Preset saved.' }))
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
                {t({ id: 'status.edit', defaultMessage: 'Edit' })}
              </Button>
              <ConfirmButton
                size="xs"
                title={t({ id: 'eunha.admin.warning_presets.delete_this_preset', defaultMessage: 'Delete this preset?' })}
                description={t({ id: 'eunha.admin.warning_presets.warnings_already_sent_with_it_are', defaultMessage: 'Warnings already sent with it are not changed.' })}
                confirmLabel={t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
                onConfirm={async () => {
                  await deleteWarningPreset(token ?? '', p.id)
                  setPresets((list) => (list ?? []).filter((x) => x.id !== p.id))
                  toast.success(t({ id: 'eunha.admin.warning_presets.preset_deleted', defaultMessage: 'Preset deleted.' }))
                }}
              >
                {t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
              </ConfirmButton>
            </div>
          ),
        )}
      </div>
    </AdminLayout>
  )
}
