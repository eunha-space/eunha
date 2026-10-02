import { useEffect, useState, type FormEvent } from 'react'
import { ArrowDown, ArrowUp } from 'lucide-react'
import { toast } from 'sonner'

import {
  createRule,
  deleteRule,
  listRules,
  moveRule,
  updateRule,
  type AdminRule,
  type RuleParams,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ConfirmButton } from '@/components/admin/admin-common.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'

type TranslationDraft = RuleParams['translations_attributes'][number]

function RuleForm({
  initial,
  submitLabel,
  onSubmit,
  onCancel,
}: {
  initial?: AdminRule
  submitLabel: string
  onSubmit: (params: RuleParams) => Promise<void>
  onCancel?: () => void
}) {
  const key = initial?.id ?? 'new'
  const [text, setText] = useState(initial?.text ?? '')
  const [hint, setHint] = useState(initial?.hint ?? '')
  const [translations, setTranslations] = useState<TranslationDraft[]>(
    () => initial?.translations.map((t) => ({ ...t })) ?? [],
  )
  const [saving, setSaving] = useState(false)
  const setTranslation = (index: number, change: Partial<TranslationDraft>) =>
    setTranslations((list) => list.map((t, i) => (i === index ? { ...t, ...change } : t)))

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setSaving(true)
    try {
      await onSubmit({ text, hint, translations_attributes: translations })
      if (!initial) {
        setText('')
        setHint('')
        setTranslations([])
      }
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <form onSubmit={submit} className="space-y-3 rounded-lg border p-3">
      <div className="space-y-1">
        <Label htmlFor={`rule-text-${key}`}>Rule</Label>
        <Textarea
          id={`rule-text-${key}`}
          rows={2}
          value={text}
          maxLength={300}
          onChange={(e) => setText(e.target.value)}
        />
        <p className="text-muted-foreground text-xs">
          Describe a rule or requirement for users on this server. Try to keep it short and
          simple.
        </p>
      </div>
      <div className="space-y-1">
        <Label htmlFor={`rule-hint-${key}`}>Additional info</Label>
        <Textarea
          id={`rule-hint-${key}`}
          rows={2}
          value={hint}
          onChange={(e) => setHint(e.target.value)}
        />
        <p className="text-muted-foreground text-xs">
          Optional. Provide more details about the rule.
        </p>
      </div>
      {translations.map((t, index) =>
        t._destroy ? null : (
          <div key={t.id ?? `new-${index}`} className="space-y-2 rounded-md border border-dashed p-2">
            <div className="flex items-end gap-2">
              <div className="w-32 space-y-1">
                <Label htmlFor={`rule-lang-${key}-${index}`}>Language</Label>
                <Input
                  id={`rule-lang-${key}-${index}`}
                  value={t.language}
                  placeholder="ko"
                  onChange={(e) => setTranslation(index, { language: e.target.value })}
                />
              </div>
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() =>
                  t.id
                    ? setTranslation(index, { _destroy: true })
                    : setTranslations((list) => list.filter((_, i) => i !== index))
                }
              >
                Remove
              </Button>
            </div>
            <Textarea
              aria-label="Translated rule"
              rows={2}
              lang={t.language}
              value={t.text}
              onChange={(e) => setTranslation(index, { text: e.target.value })}
            />
            <Textarea
              aria-label="Translated additional info"
              rows={2}
              lang={t.language}
              value={t.hint}
              placeholder="Additional info"
              onChange={(e) => setTranslation(index, { hint: e.target.value })}
            />
          </div>
        ),
      )}
      <div className="flex flex-wrap gap-2">
        <Button type="submit" size="sm" disabled={saving || !text.trim()}>
          {submitLabel}
        </Button>
        <Button
          type="button"
          size="sm"
          variant="outline"
          onClick={() =>
            setTranslations((list) => [...list, { language: '', text: '', hint: '' }])
          }
        >
          Add translation
        </Button>
        {onCancel && (
          <Button type="button" size="sm" variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
        )}
      </div>
    </form>
  )
}

/**
 * Server rules: Mastodon's `Admin::RulesController`. Reports cite them, and
 * `/api/v1/instance/rules` serves them in this order.
 */
export default function Rules() {
  const token = getToken()
  const [rules, setRules] = useState<AdminRule[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [editing, setEditing] = useState<string | null>(null)

  const load = () => {
    if (!token) return
    listRules(token)
      .then((r) => {
        setRules(r)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token])

  const move = async (id: string, direction: 'move_up' | 'move_down') => {
    try {
      setRules(await moveRule(token ?? '', id, direction))
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <AdminLayout title="Server rules" permission="manage_rules">
      <p className="text-muted-foreground mb-3 text-sm">
        Server rules are shown to people signing up and can be cited in reports.
      </p>
      <div className="mb-4">
        <RuleForm
          submitLabel="Add rule"
          onSubmit={async (params) => {
            await createRule(token ?? '', params)
            toast.success('Rule added.')
            load()
          }}
        />
      </div>
      <AdminError error={error} />
      {rules === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {rules?.length === 0 && (
        <p className="text-muted-foreground text-sm">You have not defined any server rules yet.</p>
      )}
      <ol className="space-y-2">
        {rules?.map((rule, index) =>
          editing === rule.id ? (
            <li key={rule.id}>
              <RuleForm
                initial={rule}
                submitLabel="Save changes"
                onCancel={() => setEditing(null)}
                onSubmit={async (params) => {
                  await updateRule(token ?? '', rule.id, params)
                  toast.success('Rule saved.')
                  setEditing(null)
                  load()
                }}
              />
            </li>
          ) : (
            <li key={rule.id} className="flex items-start gap-2 rounded-lg border p-3">
              <span className="text-muted-foreground w-6 shrink-0 text-sm tabular-nums">
                {index + 1}.
              </span>
              <div className="min-w-0 flex-1 space-y-1">
                <div className="text-sm font-medium">{rule.text}</div>
                {rule.hint && (
                  <p className="text-muted-foreground text-sm whitespace-pre-wrap">{rule.hint}</p>
                )}
                {rule.translations.length > 0 && (
                  <p className="text-muted-foreground text-xs">
                    Translated into {rule.translations.map((t) => t.language).join(', ')}
                  </p>
                )}
              </div>
              <Button
                size="icon-xs"
                variant="ghost"
                aria-label="Move up"
                onClick={() => void move(rule.id, 'move_up')}
              >
                <ArrowUp />
              </Button>
              <Button
                size="icon-xs"
                variant="ghost"
                aria-label="Move down"
                onClick={() => void move(rule.id, 'move_down')}
              >
                <ArrowDown />
              </Button>
              <Button size="xs" variant="outline" onClick={() => setEditing(rule.id)}>
                Edit
              </Button>
              <ConfirmButton
                size="xs"
                title="Delete this rule?"
                description="Reports that cite it keep showing it."
                confirmLabel="Delete"
                onConfirm={async () => {
                  await deleteRule(token ?? '', rule.id)
                  setRules((list) => (list ?? []).filter((r) => r.id !== rule.id))
                  toast.success('Rule deleted.')
                }}
              >
                Delete
              </ConfirmButton>
            </li>
          ),
        )}
      </ol>
    </AdminLayout>
  )
}
