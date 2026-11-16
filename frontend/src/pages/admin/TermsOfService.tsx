import { useEffect, useState, type ReactNode } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { toast } from 'sonner'

import {
  AdminApiError,
  distributeTermsOfService,
  generateTermsOfService,
  getTermsOfService,
  getTermsOfServiceDraft,
  getTermsOfServiceGenerator,
  getTermsOfServiceHistory,
  previewTermsOfService,
  saveTermsOfServiceDraft,
  testTermsOfService,
  type AdminTermsOfService,
  type TermsOfServiceGenerator,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { formatPolicyDate } from '@/lib/policy-date.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ConfirmButton } from '@/components/admin/admin-common.tsx'
import { PolicyText } from '@/components/policy-text.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Separator } from '@/components/ui/separator.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { FormSkeleton, RowsSkeleton, TextSkeleton } from '@/components/skeletons.tsx'
import { currentLocale, t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

// Mastodon's admin pages for terms of service, at its own paths, on the REST
// endpoints eunha serves for its forms. All of them are `manage_settings`'.

const TABS = [
  { to: '/admin/terms_of_service', get label() { return t({ id: 'mastodon.admin.terms_of_service.current', defaultMessage: 'Current' }) }, end: true },
  { to: '/admin/terms_of_service/draft', get label() { return t({ id: 'mastodon.admin.terms_of_service.draft', defaultMessage: 'Draft' }) } },
  { to: '/admin/terms_of_service/history', get label() { return t({ id: 'mastodon.admin.terms_of_service.history', defaultMessage: 'History' }) } },
]

function Layout({ title, actions, children }: { title: string; actions?: ReactNode; children: ReactNode }) {
  return (
    <AdminLayout title={title} permission="manage_settings" sub={TABS} actions={actions}>
      {children}
    </AdminLayout>
  )
}

/** `l(date)` of a timestamp's day. */
function day(value: string | null): string {
  return value ? formatPolicyDate(value.slice(0, 10)) : ''
}

/** Loads one thing for a page, with its error. */
function useLoad<T>(load: (token: string) => Promise<T>, deps: unknown[]) {
  const token = getToken()
  const [value, setValue] = useState<T | null | undefined>(undefined)
  const [error, setError] = useState<string | null>(null)
  useEffect(() => {
    if (!token) return
    let cancelled = false
    setValue(undefined)
    setError(null)
    load(token)
      .then((v) => !cancelled && setValue(v))
      .catch((e) => {
        if (cancelled) return
        // A 404 is "there is none", which each page says in its own words.
        if (e instanceof AdminApiError && e.status === 404) setValue(null)
        else setError(errorMessage(e))
      })
    return () => {
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [token, ...deps])
  return { token, value, setValue, error }
}

// ── /admin/terms_of_service ─────────────────────────────────────────────────

export function AdminTermsOfServiceIndex() {
  const { value: terms, error } = useLoad(getTermsOfService, [])

  return (
    <Layout
      title={t({ id: 'terms_of_service.title', defaultMessage: 'Terms of Service' })}
      actions={
        <Button size="sm" variant="outline" render={<Link to="/admin/terms_of_service/generate" />}>
          {t({ id: 'mastodon.admin.terms_of_service.generate', defaultMessage: 'Use template' })}
        </Button>
      }
    >
      <AdminError error={error} />
      {terms === undefined && !error && <TextSkeleton />}
      {terms === null && (
        <div className="space-y-3">
          <p className="text-sm">
            {t({ id: 'mastodon.admin.terms_of_service.no_terms_of_service_html', defaultMessage: 'You don\'\'t currently have any terms of service configured. Terms of service are meant to provide clarity and protect you from potential liabilities in disputes with your users.' })}
          </p>
          <div className="flex gap-2">
            <Button size="sm" render={<Link to="/admin/terms_of_service/draft" />}>
              {t({ id: 'mastodon.admin.terms_of_service.create', defaultMessage: 'Use your own' })}
            </Button>
            <Button size="sm" variant="outline" render={<Link to="/admin/terms_of_service/generate" />}>
              {t({ id: 'mastodon.admin.terms_of_service.generate', defaultMessage: 'Use template' })}
            </Button>
          </div>
        </div>
      )}
      {terms && (
        <div className="space-y-4">
          <div className="text-muted-foreground flex flex-wrap items-center gap-2 text-sm">
            <Badge variant="secondary">
              {terms.effective || !terms.effective_date
                ? t({ id: 'mastodon.admin.terms_of_service.live', defaultMessage: 'Live' })
                : t({ id: 'mastodon.admin.terms_of_service.going_live_on_html', defaultMessage: 'Live, effective {date}' }, { date: formatPolicyDate(terms.effective_date) })}
            </Badge>
            {terms.published_at && <span>{t({ id: 'mastodon.admin.terms_of_service.published_on_html', defaultMessage: 'Published on {date}' }, { date: day(terms.published_at) })}</span>}
            <span>·</span>
            {terms.notification_sent_at ? (
              <span>{t({ id: 'mastodon.admin.terms_of_service.notified_on_html', defaultMessage: 'Users notified on {date}' }, { date: day(terms.notification_sent_at) })}</span>
            ) : (
              terms.id && (
                <Link className="text-primary" to={`/admin/terms_of_service/${terms.id}/preview`}>
                  {t({ id: 'mastodon.admin.terms_of_service.notify_users', defaultMessage: 'Notify users' })}
                </Link>
              )
            )}
          </div>
          <Card>
            <CardContent className="py-4">
              <PolicyText html={terms.text_html} />
            </CardContent>
          </Card>
          <Separator />
          <h2 className="font-semibold">{t({ id: 'mastodon.admin.terms_of_service.changelog', defaultMessage: 'What\'\'s changed' })}</h2>
          <PolicyText html={terms.changelog_html} />
        </div>
      )}
    </Layout>
  )
}

// ── /admin/terms_of_service/draft ───────────────────────────────────────────

export function AdminTermsOfServiceDraft() {
  const navigate = useNavigate()
  const { token, value: draft, error } = useLoad(getTermsOfServiceDraft, [])
  const [text, setText] = useState('')
  const [changelog, setChangelog] = useState('')
  const [effectiveDate, setEffectiveDate] = useState('')
  const [busy, setBusy] = useState(false)
  const [refusal, setRefusal] = useState<string | null>(null)

  useEffect(() => {
    if (!draft) return
    setText(draft.text)
    setChangelog(draft.changelog)
    setEffectiveDate(draft.effective_date ?? '')
  }, [draft])

  const save = async (actionType: 'save_draft' | 'publish') => {
    if (!token) return
    setBusy(true)
    setRefusal(null)
    try {
      const saved = await saveTermsOfServiceDraft(token, {
        text,
        changelog,
        effective_date: effectiveDate,
        action_type: actionType,
      })
      if (saved.published_at) {
        toast.success(t({ id: 'eunha.admin.terms_of_service.published_the_terms_of_service', defaultMessage: 'Published the terms of service.' }))
        navigate('/admin/terms_of_service')
      } else {
        toast.success(t({ id: 'eunha.admin.terms_of_service.saved_the_draft', defaultMessage: 'Saved the draft.' }))
      }
    } catch (e) {
      setRefusal(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Layout title={t({ id: 'terms_of_service.title', defaultMessage: 'Terms of Service' })}>
      <AdminError error={error} />
      {draft === undefined && !error && <FormSkeleton fields={3} />}
      {draft && (
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault()
            void save('save_draft')
          }}
        >
          {refusal && <p className="text-destructive text-sm">{refusal}</p>}
          <div className="space-y-1">
            <Label htmlFor="tos-text">{t({ id: 'terms_of_service.title', defaultMessage: 'Terms of Service' })}</Label>
            <p className="text-muted-foreground text-xs">{t({ id: 'mastodon.simple_form.hints.terms_of_service.changelog', defaultMessage: 'Can be structured with Markdown syntax.' })}</p>
            <Textarea
              id="tos-text"
              rows={14}
              className="font-mono text-xs"
              value={text}
              onChange={(e) => setText(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="tos-changelog">{t({ id: 'mastodon.simple_form.labels.terms_of_service.changelog', defaultMessage: 'What\'\'s changed?' })}</Label>
            <p className="text-muted-foreground text-xs">{t({ id: 'mastodon.simple_form.hints.terms_of_service.changelog', defaultMessage: 'Can be structured with Markdown syntax.' })}</p>
            <Textarea
              id="tos-changelog"
              rows={6}
              className="font-mono text-xs"
              value={changelog}
              onChange={(e) => setChangelog(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="tos-effective-date">{t({ id: 'mastodon.simple_form.labels.terms_of_service.effective_date', defaultMessage: 'Effective date' })}</Label>
            <p className="text-muted-foreground text-xs">
              {t({ id: 'mastodon.simple_form.hints.terms_of_service.effective_date', defaultMessage: 'A reasonable timeframe can range anywhere from 10 to 30 days from the date you notify your users.' })}
            </p>
            <Input
              id="tos-effective-date"
              type="date"
              className="w-auto"
              value={effectiveDate}
              onChange={(e) => setEffectiveDate(e.target.value)}
            />
          </div>
          <div className="flex flex-wrap gap-2">
            <Button type="submit" variant="secondary" disabled={busy}>
              {t({ id: 'mastodon.admin.terms_of_service.save_draft', defaultMessage: 'Save draft' })}
            </Button>
            <ConfirmButton
              variant="default"
              destructive={false}
              disabled={busy}
              title={t({ id: 'eunha.admin.terms_of_service.publish_these_terms_of_service', defaultMessage: 'Publish these terms of service?' })}
              description={t({ id: 'eunha.admin.terms_of_service.published_terms_cannot_be_edited_they', defaultMessage: 'Published terms cannot be edited. They become the current terms on their effective date.' })}
              confirmLabel={t({ id: 'compose.publish', defaultMessage: 'Publish' })}
              onConfirm={() => save('publish')}
            >
              {t({ id: 'compose.publish', defaultMessage: 'Publish' })}
            </ConfirmButton>
          </div>
        </form>
      )}
    </Layout>
  )
}

// ── /admin/terms_of_service/history ─────────────────────────────────────────

export function AdminTermsOfServiceHistory() {
  const { value: history, error } = useLoad(getTermsOfServiceHistory, [])

  return (
    <Layout title={t({ id: 'terms_of_service.title', defaultMessage: 'Terms of Service' })}>
      <AdminError error={error} />
      {history === undefined && !error && <RowsSkeleton />}
      {history?.length === 0 && (
        <p className="text-muted-foreground text-sm">
          {t({ id: 'mastodon.admin.terms_of_service.no_history', defaultMessage: 'There are no recorded changes of the terms of service yet.' })}
        </p>
      )}
      {!!history?.length && (
        <ol className="divide-y rounded-lg border">
          {history.map((t) => (
            <li key={t.id ?? t.effective_date ?? ''} className="space-y-1 p-3">
              <h2 className="text-sm font-semibold">
                {t.effective_date ? (
                  <Link className="text-primary" to={`/terms-of-service/${t.effective_date}`}>
                    {day(t.published_at)}
                  </Link>
                ) : (
                  day(t.published_at)
                )}
              </h2>
              <PolicyText html={t.changelog_html} />
            </li>
          ))}
        </ol>
      )}
    </Layout>
  )
}

// ── /admin/terms_of_service/generate ────────────────────────────────────────

const GENERATOR_FIELDS: { key: keyof TermsOfServiceGenerator; label: string; hint: string }[] = [
  {
    key: 'domain',
    get label() { return t({ id: 'mastodon.admin.instances.by_domain', defaultMessage: 'Domain' }) },
    get hint() { return t({ id: 'mastodon.simple_form.hints.terms_of_service_generator.domain', defaultMessage: 'Unique identification of the online service you are providing.' }) },
  },
  {
    key: 'min_age',
    get label() { return t({ id: 'mastodon.simple_form.labels.terms_of_service_generator.min_age', defaultMessage: 'Minimum age' }) },
    get hint() { return t({ id: 'mastodon.simple_form.hints.terms_of_service_generator.min_age', defaultMessage: 'Should not be below the minimum age required by the laws of your jurisdiction.' }) },
  },
  {
    key: 'jurisdiction',
    get label() { return t({ id: 'mastodon.simple_form.labels.terms_of_service_generator.jurisdiction', defaultMessage: 'Legal jurisdiction' }) },
    get hint() { return t({ id: 'eunha.admin.terms_of_service.list_the_country_where_whoever_pays', defaultMessage: 'List the country where whoever pays the bills lives. If it\'\'s a company or other entity, list the country where it\'\'s incorporated, and the city, region, territory or state as appropriate.' }) },
  },
  {
    key: 'choice_of_law',
    get label() { return t({ id: 'mastodon.simple_form.labels.terms_of_service_generator.choice_of_law', defaultMessage: 'Choice of Law' }) },
    get hint() { return t({ id: 'mastodon.simple_form.hints.terms_of_service_generator.choice_of_law', defaultMessage: 'City, region, territory or state the internal substantive laws of which shall govern any and all claims.' }) },
  },
  {
    key: 'admin_email',
    get label() { return t({ id: 'mastodon.simple_form.labels.terms_of_service_generator.admin_email', defaultMessage: 'Email address for legal notices' }) },
    get hint() { return t({ id: 'mastodon.simple_form.hints.terms_of_service_generator.admin_email', defaultMessage: 'Legal notices include counternotices, court orders, takedown requests, and law enforcement requests.' }) },
  },
  {
    key: 'dmca_address',
    get label() { return t({ id: 'mastodon.simple_form.labels.terms_of_service_generator.dmca_address', defaultMessage: 'Physical address for DMCA/copyright notices' }) },
    get hint() { return t({ id: 'eunha.admin.terms_of_service.for_us_operators_use_the_address', defaultMessage: 'For US operators, use the address registered in the DMCA Designated Agent Directory.' }) },
  },
  {
    key: 'dmca_email',
    get label() { return t({ id: 'mastodon.simple_form.labels.terms_of_service_generator.dmca_email', defaultMessage: 'Email address for DMCA/copyright notices' }) },
    get hint() { return t({ id: 'eunha.admin.terms_of_service.can_be_the_same_email_used', defaultMessage: 'Can be the same email used for "Email address for legal notices" above.' }) },
  },
  {
    key: 'arbitration_address',
    get label() { return t({ id: 'mastodon.simple_form.labels.terms_of_service_generator.arbitration_address', defaultMessage: 'Physical address for arbitration notices' }) },
    get hint() { return t({ id: 'eunha.admin.terms_of_service.can_be_the_same_as_physical', defaultMessage: 'Can be the same as Physical address above, or "N/A" if using email.' }) },
  },
  {
    key: 'arbitration_website',
    get label() { return t({ id: 'mastodon.simple_form.labels.terms_of_service_generator.arbitration_website', defaultMessage: 'Website for submitting arbitration notices' }) },
    get hint() { return t({ id: 'eunha.admin.terms_of_service.can_be_a_web_form_or', defaultMessage: 'Can be a web form, or "N/A" if using email.' }) },
  },
]

export function AdminTermsOfServiceGenerate() {
  const navigate = useNavigate()
  const { token, value: defaults, error } = useLoad(getTermsOfServiceGenerator, [])
  const [fields, setFields] = useState<Partial<TermsOfServiceGenerator>>({})
  const [busy, setBusy] = useState(false)
  const [refusal, setRefusal] = useState<string | null>(null)

  useEffect(() => {
    if (defaults) setFields(defaults)
  }, [defaults])

  const generate = async () => {
    if (!token) return
    setBusy(true)
    setRefusal(null)
    try {
      const params = Object.fromEntries(
        GENERATOR_FIELDS.map(({ key }) => [key, fields[key] ?? null]),
      ) as unknown as TermsOfServiceGenerator
      await generateTermsOfService(token, params)
      toast.success(t({ id: 'eunha.admin.terms_of_service.generated_a_draft_from_the_template', defaultMessage: 'Generated a draft from the template.' }))
      navigate('/admin/terms_of_service/draft')
    } catch (e) {
      setRefusal(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Layout title={t({ id: 'mastodon.admin.terms_of_service.generates.title', defaultMessage: 'Terms of Service Setup' })}>
      <AdminError error={error} />
      {defaults && (
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault()
            void generate()
          }}
        >
          <p className="text-sm">
            {t({ id: 'mastodon.admin.terms_of_service.generates.explanation_html', defaultMessage: 'The terms of service template provided is for informational purposes only, and should not be construed as legal advice on any subject matter. Please consult with your own legal counsel on your situation and specific legal questions you have.' })}
          </p>
          <p className="text-sm">
            <FormattedMessage
              id='mastodon.admin.terms_of_service.generates.chance_to_review_html'
              defaultMessage='<strong>The generated terms of service will not be published automatically.</strong> You will have a chance to review the results. Please fill in the necessary details to proceed.'
              values={{ strong: (chunks) => <strong>{chunks}</strong> }}
            />
          </p>
          <Separator />
          {refusal && <p className="text-destructive text-sm">{refusal}</p>}
          {GENERATOR_FIELDS.map(({ key, label, hint }) => (
            <div key={key} className="space-y-1">
              <Label htmlFor={`tos-gen-${key}`}>{label}</Label>
              <p className="text-muted-foreground text-xs">{hint}</p>
              <Input
                id={`tos-gen-${key}`}
                value={fields[key] ?? ''}
                onChange={(e) => setFields((f) => ({ ...f, [key]: e.target.value }))}
              />
            </div>
          ))}
          <Button type="submit" disabled={busy}>
            {busy ? t({ id: 'eunha.common.generating', defaultMessage: 'Generating…' }) : t({ id: 'mastodon.admin.terms_of_service.generates.action', defaultMessage: 'Generate' })}
          </Button>
        </form>
      )}
    </Layout>
  )
}

// ── /admin/terms_of_service/:id/preview ─────────────────────────────────────

export function AdminTermsOfServicePreview() {
  const { id = '' } = useParams<{ id: string }>()
  const navigate = useNavigate()
  const { token, value: preview, error } = useLoad((t) => previewTermsOfService(t, id), [id])
  const [testing, setTesting] = useState(false)

  const count = preview?.user_count ?? 0
  const terms: AdminTermsOfService | undefined = preview?.terms_of_service

  return (
    <Layout title={t({ id: 'mastodon.admin.terms_of_service.preview.title', defaultMessage: 'Preview terms of service notification' })}>
      <AdminError error={error} />
      {preview === undefined && !error && <TextSkeleton />}
      {preview && terms && (
        <div className="space-y-4">
          <p className="text-sm">
            <FormattedMessage
              id='mastodon.admin.terms_of_service.preview.explanation_html'
              defaultMessage='The email will be sent to <strong>{display_count} users</strong> who have signed up before {date}. The following text will be included in the e-mail:'
              values={{
                display_count: count.toLocaleString(currentLocale()),
                date: day(terms.published_at),
                strong: (chunks) => <strong>{chunks}</strong>,
              }}
            />
          </p>
          <Card>
            <CardContent className="py-4">
              <PolicyText html={terms.changelog_html} />
            </CardContent>
          </Card>
          <div className="flex flex-wrap gap-2">
            <Button
              variant="secondary"
              disabled={testing}
              onClick={async () => {
                if (!token) return
                setTesting(true)
                try {
                  await testTermsOfService(token, id)
                  toast.success(t({ id: 'eunha.admin.terms_of_service.sent_a_preview_to_your_email', defaultMessage: 'Sent a preview to your email address.' }))
                } catch (e) {
                  toast.error(errorMessage(e))
                } finally {
                  setTesting(false)
                }
              }}
            >
              {t({ id: 'eunha.admin.terms_of_service.send_preview_to_yourself', defaultMessage: 'Send preview to yourself' })}
            </Button>
            <ConfirmButton
              variant="default"
              destructive={false}
              title={t({ id: 'mastodon.admin.accounts.are_you_sure', defaultMessage: 'Are you sure?' })}
              description={t(
                { id: 'eunha.admin.terms_of_service.every_user_emailed', defaultMessage: 'Every one of the {count, number} users will be emailed. Users who were not active in the year before these terms were published will be shown them when they next sign in instead.' },
                { count },
              )}
              confirmLabel={t({ id: 'eunha.admin.terms_of_service.send_emails', defaultMessage: '{count, plural, one {Send # email} other {Send # emails}}' }, { count })}
              onConfirm={async () => {
                if (!token) return
                await distributeTermsOfService(token, id)
                toast.success(t({ id: 'eunha.admin.terms_of_service.notifying_users', defaultMessage: 'Notifying users.' }))
                navigate('/admin/terms_of_service')
              }}
            >
              {t({ id: 'eunha.admin.terms_of_service.send_emails', defaultMessage: '{count, plural, one {Send # email} other {Send # emails}}' }, { count })}
            </ConfirmButton>
          </div>
        </div>
      )}
    </Layout>
  )
}
