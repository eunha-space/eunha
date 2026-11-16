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
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

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
    () => initial?.translations.map((tr) => ({ ...tr })) ?? [],
  )
  const [saving, setSaving] = useState(false)
  const setTranslation = (index: number, change: Partial<TranslationDraft>) =>
    setTranslations((list) => list.map((tr, i) => (i === index ? { ...tr, ...change } : tr)))

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
        <Label htmlFor={`rule-text-${key}`}>{t({ id: 'mastodon.simple_form.labels.rule.text', defaultMessage: 'Rule' })}</Label>
        <Textarea
          id={`rule-text-${key}`}
          rows={2}
          value={text}
          maxLength={300}
          onChange={(e) => setText(e.target.value)}
        />
        <p className="text-muted-foreground text-xs">
          {t({ id: 'eunha.admin.rules.describe_a_rule_or_requirement_for', defaultMessage: 'Describe a rule or requirement for users on this server. Try to keep it short and simple.' })}
        </p>
      </div>
      <div className="space-y-1">
        <Label htmlFor={`rule-hint-${key}`}>{t({ id: 'eunha.admin.rules.additional_info', defaultMessage: 'Additional info' })}</Label>
        <Textarea
          id={`rule-hint-${key}`}
          rows={2}
          value={hint}
          onChange={(e) => setHint(e.target.value)}
        />
        <p className="text-muted-foreground text-xs">
          {t({ id: 'eunha.admin.rules.optional_provide_more_details_about_the', defaultMessage: 'Optional. Provide more details about the rule.' })}
        </p>
      </div>
      {translations.map((tr, index) =>
        tr._destroy ? null : (
          <div key={tr.id ?? `new-${index}`} className="space-y-2 rounded-md border border-dashed p-2">
            <div className="flex items-end gap-2">
              <div className="w-32 space-y-1">
                <Label htmlFor={`rule-lang-${key}-${index}`}>{t({ id: 'about.language_label', defaultMessage: 'Language' })}</Label>
                <Input
                  id={`rule-lang-${key}-${index}`}
                  value={tr.language}
                  placeholder="ko"
                  onChange={(e) => setTranslation(index, { language: e.target.value })}
                />
              </div>
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() =>
                  tr.id
                    ? setTranslation(index, { _destroy: true })
                    : setTranslations((list) => list.filter((_, i) => i !== index))
                }
              >
                {t({ id: 'tag.remove', defaultMessage: 'Remove' })}
              </Button>
            </div>
            <Textarea
              aria-label={t({ id: 'eunha.admin.rules.translated_rule', defaultMessage: 'Translated rule' })}
              rows={2}
              lang={tr.language}
              value={tr.text}
              onChange={(e) => setTranslation(index, { text: e.target.value })}
            />
            <Textarea
              aria-label={t({ id: 'eunha.admin.rules.translated_additional_info', defaultMessage: 'Translated additional info' })}
              rows={2}
              lang={tr.language}
              value={tr.hint}
              placeholder={t({ id: 'eunha.admin.rules.additional_info', defaultMessage: 'Additional info' })}
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
          {t({ id: 'mastodon.admin.rules.add_translation', defaultMessage: 'Add translation' })}
        </Button>
        {onCancel && (
          <Button type="button" size="sm" variant="ghost" onClick={onCancel}>
            {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
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
    <AdminLayout title={t({ id: 'about.rules', defaultMessage: 'Server rules' })} permission="manage_rules">
      <p className="text-muted-foreground mb-3 text-sm">
        {t({ id: 'eunha.admin.rules.server_rules_are_shown_to_people', defaultMessage: 'Server rules are shown to people signing up and can be cited in reports.' })}
      </p>
      <div className="mb-4">
        <RuleForm
          submitLabel={t({ id: 'mastodon.admin.rules.add_new', defaultMessage: 'Add rule' })}
          onSubmit={async (params) => {
            await createRule(token ?? '', params)
            toast.success(t({ id: 'eunha.admin.rules.rule_added', defaultMessage: 'Rule added.' }))
            load()
          }}
        />
      </div>
      <AdminError error={error} />
      {rules === null && !error && <RowsSkeleton action />}
      {rules?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.rules.you_have_not_defined_any_server', defaultMessage: 'You have not defined any server rules yet.' })}</p>
      )}
      <ol className="space-y-2">
        {rules?.map((rule, index) =>
          editing === rule.id ? (
            <li key={rule.id}>
              <RuleForm
                initial={rule}
                submitLabel={t({ id: 'subscribed_languages.save', defaultMessage: 'Save changes' })}
                onCancel={() => setEditing(null)}
                onSubmit={async (params) => {
                  await updateRule(token ?? '', rule.id, params)
                  toast.success(t({ id: 'eunha.admin.rules.rule_saved', defaultMessage: 'Rule saved.' }))
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
                    {t({ id: 'eunha.admin.rules.translated_into', defaultMessage: 'Translated into {languages}' }, { languages: rule.translations.map((tr) => tr.language).join(', ') })}
                  </p>
                )}
              </div>
              <Button
                size="icon-xs"
                variant="ghost"
                aria-label={t({ id: 'mastodon.admin.rules.move_up', defaultMessage: 'Move up' })}
                onClick={() => void move(rule.id, 'move_up')}
              >
                <ArrowUp />
              </Button>
              <Button
                size="icon-xs"
                variant="ghost"
                aria-label={t({ id: 'mastodon.admin.rules.move_down', defaultMessage: 'Move down' })}
                onClick={() => void move(rule.id, 'move_down')}
              >
                <ArrowDown />
              </Button>
              <Button size="xs" variant="outline" onClick={() => setEditing(rule.id)}>
                {t({ id: 'status.edit', defaultMessage: 'Edit' })}
              </Button>
              <ConfirmButton
                size="xs"
                title={t({ id: 'eunha.admin.rules.delete_this_rule', defaultMessage: 'Delete this rule?' })}
                description={t({ id: 'eunha.admin.rules.reports_that_cite_it_keep_showing', defaultMessage: 'Reports that cite it keep showing it.' })}
                confirmLabel={t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
                onConfirm={async () => {
                  await deleteRule(token ?? '', rule.id)
                  setRules((list) => (list ?? []).filter((r) => r.id !== rule.id))
                  toast.success(t({ id: 'eunha.admin.rules.rule_deleted', defaultMessage: 'Rule deleted.' }))
                }}
              >
                {t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
              </ConfirmButton>
            </li>
          ),
        )}
      </ol>
    </AdminLayout>
  )
}
