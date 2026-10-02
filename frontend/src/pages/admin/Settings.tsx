import { useEffect, useState, type FormEvent } from 'react'
import { Navigate, useParams } from 'react-router-dom'
import { toast } from 'sonner'

import {
  deleteSiteUpload,
  getAdminSettings,
  updateAdminSettings,
  type AdminSettings,
  type SiteUpload,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ChoiceSelect, ConfirmButton } from '@/components/admin/admin-common.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'

type Key = Exclude<keyof AdminSettings, 'overridden'>

type Field =
  | { key: Key; kind: 'text' | 'email' | 'url' | 'number'; label: string; hint?: string }
  | { key: Key; kind: 'textarea'; label: string; hint?: string; rows?: number }
  | { key: Key; kind: 'switch'; label: string; hint?: string }
  | { key: Key; kind: 'select'; label: string; hint?: string; items: Record<string, string> }
  | { key: Key; kind: 'upload'; label: string; hint?: string }

const FEED_ACCESS = {
  public: 'Everyone',
  authenticated: 'Authenticated users only',
  disabled: 'Require specific user role',
}

const AUDIENCES = {
  disabled: 'To no one',
  users: 'To logged-in local users',
  all: 'To everyone',
}

/**
 * Mastodon's settings pages, each with the keys its form posts, labelled and
 * hinted as `simple_form.en.yml` labels and hints them.
 */
const PAGES: Record<string, { title: string; preamble: string; fields: Field[] }> = {
  branding: {
    title: 'Branding',
    preamble:
      "Your server's branding differentiates it from other servers in the network. This information may be displayed across a variety of environments.",
    fields: [
      { key: 'site_title', kind: 'text', label: 'Server name', hint: 'How people may refer to your server besides its domain name.' },
      { key: 'site_contact_username', kind: 'text', label: 'Contact username', hint: 'How people can reach you on the fediverse.' },
      { key: 'site_contact_email', kind: 'email', label: 'Contact e-mail', hint: 'How people can reach you for legal or support inquiries.' },
      { key: 'site_short_description', kind: 'textarea', label: 'Server description', hint: 'A short description to help uniquely identify your server. Who is running it, who is it for?', rows: 2 },
      { key: 'thumbnail', kind: 'upload', label: 'Server thumbnail', hint: 'A roughly 2:1 image displayed alongside your server information.' },
      { key: 'thumbnail_description', kind: 'text', label: 'Thumbnail alt text', hint: 'A description of the image to help people with visual impairments understand its content.' },
      { key: 'favicon', kind: 'upload', label: 'Favicon', hint: 'WEBP, PNG, GIF or JPG. Overrides the default favicon with a custom icon.' },
      { key: 'app_icon', kind: 'upload', label: 'App icon', hint: 'WEBP, PNG, GIF or JPG. Overrides the default app icon on mobile devices with a custom icon.' },
      {
        key: 'landing_page',
        kind: 'select',
        label: 'Landing page for new visitors',
        items: { trends: 'Trending', overview: 'Overview', local_feed: 'Local live feed', about: 'About page' },
      },
    ],
  },
  about: {
    title: 'About',
    preamble: 'Provide in-depth information about how the server is operated, moderated, funded.',
    fields: [
      { key: 'site_extended_description', kind: 'textarea', label: 'Extended description', hint: 'Any additional information that may be useful to visitors and your users. Can be structured with Markdown syntax.', rows: 8 },
      { key: 'show_domain_blocks', kind: 'select', label: 'Show domain blocks', items: AUDIENCES },
      { key: 'show_domain_blocks_rationale', kind: 'select', label: 'Show why domains were blocked', items: AUDIENCES },
      { key: 'status_page_url', kind: 'url', label: 'Status page URL', hint: 'URL of a page where people can see the status of this server during an outage.' },
      { key: 'site_terms', kind: 'textarea', label: 'Privacy Policy', hint: 'Use your own privacy policy or leave blank to use the default. Can be structured with Markdown syntax.', rows: 8 },
    ],
  },
  registrations: {
    title: 'Registrations',
    preamble: 'Control who can create an account on your server.',
    fields: [
      { key: 'min_age', kind: 'number', label: 'Minimum age requirement', hint: 'Users will be asked to confirm their date of birth during sign-up.' },
      {
        key: 'registrations_mode',
        kind: 'select',
        label: 'Who can sign-up',
        hint: 'We recommend using “Approval required for sign up” unless you are confident your moderation team can handle spam and malicious registrations in a timely fashion.',
        items: { open: 'Anyone can sign up', approved: 'Approval required for sign up', none: 'Nobody can sign up' },
      },
      { key: 'require_invite_text', kind: 'switch', label: 'Require a reason to join', hint: 'When sign-ups require manual approval, make the “Why do you want to join?” text input mandatory rather than optional.' },
      { key: 'captcha_enabled', kind: 'switch', label: 'Require new users to solve a CAPTCHA', hint: 'Saved for a Mastodon on the same database; eunha has no CAPTCHA to show.' },
      { key: 'closed_registrations_message', kind: 'textarea', label: 'Custom message when sign-ups are not available', hint: 'Displayed when sign-ups are closed.', rows: 3 },
    ],
  },
  discovery: {
    title: 'Discovery',
    preamble:
      'Surfacing interesting content is instrumental in onboarding new users who may not know anyone. Control how various discovery features work on your server.',
    fields: [
      { key: 'trends', kind: 'switch', label: 'Enable trends', hint: 'Trends show which posts, hashtags and news stories are gaining traction on your server.' },
      { key: 'trendable_by_default', kind: 'switch', label: 'Allow trends without prior review', hint: 'Skip manual review of trending content. Individual items can still be removed from trends after the fact.' },
      { key: 'local_live_feed_access', kind: 'select', label: 'Access to live feeds featuring local posts', items: FEED_ACCESS },
      { key: 'remote_live_feed_access', kind: 'select', label: 'Access to live feeds featuring remote posts', items: FEED_ACCESS },
      {
        key: 'local_topic_feed_access',
        kind: 'select',
        label: 'Access to hashtag and link feeds featuring local posts',
        items: { public: FEED_ACCESS.public, authenticated: FEED_ACCESS.authenticated },
      },
      { key: 'remote_topic_feed_access', kind: 'select', label: 'Access to hashtag and link feeds featuring remote posts', items: FEED_ACCESS },
      { key: 'noindex', kind: 'switch', label: 'Opt users out of search engine indexing by default', hint: 'Affects all users who have not changed this setting themselves.' },
      { key: 'allow_referrer_origin', kind: 'switch', label: 'Allow external websites to see your server as the source of traffic' },
      { key: 'activity_api_enabled', kind: 'switch', label: 'Publish aggregate statistics about user activity in the API', hint: 'Counts of locally published posts, active users, and new registrations in weekly buckets.' },
      { key: 'peers_api_enabled', kind: 'switch', label: 'Publish list of discovered servers in the API', hint: 'A list of domain names this server has encountered in the fediverse.' },
      { key: 'authorized_fetch', kind: 'switch', label: 'Require authentication from federated servers', hint: 'Enables stricter enforcement of both user-level and server-level blocks, at the cost of performance and reach.' },
      { key: 'bootstrap_timeline_accounts', kind: 'text', label: 'Always recommend these accounts to new users', hint: "These accounts will be pinned to the top of new users' follow recommendations. Provide a comma-separated list of accounts." },
      { key: 'profile_directory', kind: 'switch', label: 'Enable profile directory', hint: 'The profile directory lists all users who have opted-in to be discoverable.' },
      { key: 'wrapstodon', kind: 'switch', label: 'Enable Wrapstodon', hint: 'Offer local users to generate a playful summary of their use during the year.' },
    ],
  },
  content_retention: {
    title: 'Content retention',
    preamble: 'Control how user-generated content is stored on this server.',
    fields: [
      { key: 'media_cache_retention_period', kind: 'number', label: 'Media cache retention period', hint: 'Days. Media from remote posts and link preview images are forgotten after this many days; blank keeps them.' },
      { key: 'content_cache_retention_period', kind: 'number', label: 'Remote content retention period', hint: 'Days. All posts from other servers, boosts and replies included, are deleted after this many days, whatever local users did with them. Intended for special purpose servers.' },
      { key: 'backups_retention_period', kind: 'number', label: 'User archive retention period', hint: 'Days after which archives users generated are deleted.' },
    ],
  },
  appearance: {
    title: 'Appearance',
    preamble: 'Customize the web interface.',
    fields: [
      { key: 'theme', kind: 'select', label: 'Default theme', items: { default: 'Default' } },
      { key: 'custom_css', kind: 'textarea', label: 'Custom CSS', hint: 'You can apply custom styles on the web version of this server.', rows: 10 },
      { key: 'mascot', kind: 'upload', label: 'Custom mascot (legacy)', hint: 'Overrides the illustration in the advanced web interface.' },
    ],
  },
}

const SUB = Object.entries(PAGES).map(([page, p]) => ({
  to: `/admin/settings/${page}`,
  label: p.title,
}))

type Draft = Partial<Record<Key, string | boolean | File | null>>

function initialDraft(fields: Field[], settings: AdminSettings): Draft {
  const draft: Draft = {}
  for (const field of fields) {
    if (field.kind === 'upload') continue
    const value = settings[field.key]
    draft[field.key] =
      typeof value === 'boolean' ? value : value === null || value === undefined ? '' : String(value)
  }
  return draft
}

function UploadField({
  field,
  current,
  file,
  onFile,
  onRemoved,
}: {
  field: Field
  current: SiteUpload | null
  file: File | null
  onFile: (file: File | null) => void
  onRemoved: () => void
}) {
  const token = getToken()
  return (
    <div className="space-y-2">
      {current?.url && (
        <div className="flex items-center gap-3">
          <img src={current.url} alt="" className="h-12 max-w-40 rounded border object-contain" />
          <ConfirmButton
            size="xs"
            title={`Remove the ${field.label.toLowerCase()}?`}
            description="The default is shown again."
            confirmLabel="Remove"
            onConfirm={async () => {
              await deleteSiteUpload(token ?? '', current.id)
              toast.success('Removed.')
              onRemoved()
            }}
          >
            Remove
          </ConfirmButton>
        </div>
      )}
      <Input
        id={`setting-${field.key}`}
        type="file"
        accept="image/jpeg,image/png,image/gif,image/webp"
        onChange={(e) => onFile(e.target.files?.[0] ?? null)}
      />
      {file && <p className="text-muted-foreground text-xs">{file.name} will be uploaded on save.</p>}
    </div>
  )
}

/**
 * The server settings: Mastodon's `Admin::Settings::*Controller` pages over
 * `Form::AdminSettings`. Each page saves its own keys and no others.
 */
export default function Settings() {
  const { page = '' } = useParams()
  const token = getToken()
  const [settings, setSettings] = useState<AdminSettings | null>(null)
  const [draft, setDraft] = useState<Draft>({})
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const def = PAGES[page]

  const load = () => {
    if (!token) return
    getAdminSettings(token)
      .then((s) => {
        setSettings(s)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token])
  useEffect(() => {
    if (settings && def) setDraft(initialDraft(def.fields, settings))
  }, [settings, def])

  if (!def) return <Navigate to="/admin/settings/branding" replace />

  const save = async (e: FormEvent) => {
    e.preventDefault()
    setSaving(true)
    try {
      const values: Record<string, string | boolean | File> = {}
      for (const field of def.fields) {
        const value = draft[field.key]
        if (field.kind === 'upload') {
          if (value instanceof File) values[field.key] = value
        } else if (value !== undefined && value !== null) {
          values[field.key] = value
        }
      }
      setSettings(await updateAdminSettings(token ?? '', values))
      toast.success('Changes successfully saved!')
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  const set = (key: Key, value: string | boolean | File | null) =>
    setDraft((d) => ({ ...d, [key]: value }))

  return (
    <AdminLayout title={`Server settings · ${def.title}`} permission="manage_settings" sub={SUB}>
      <AdminError error={error} />
      {settings === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {settings && (
        <form onSubmit={save} className="max-w-2xl space-y-5">
          <p className="text-muted-foreground text-sm">{def.preamble}</p>
          {def.fields.map((field) => {
            const overridden = settings.overridden.includes(field.key)
            const id = `setting-${field.key}`
            const value = draft[field.key]
            return (
              <div key={field.key} className="space-y-1.5">
                {field.kind === 'switch' ? (
                  <Label className="text-sm font-medium">
                    <Switch
                      checked={value === true}
                      disabled={overridden}
                      onCheckedChange={(on) => set(field.key, on)}
                    />
                    {field.label}
                  </Label>
                ) : (
                  <Label htmlFor={id}>{field.label}</Label>
                )}
                {field.kind === 'textarea' && (
                  <Textarea
                    id={id}
                    rows={field.rows ?? 4}
                    className="resize-y font-mono text-sm"
                    value={typeof value === 'string' ? value : ''}
                    onChange={(e) => set(field.key, e.target.value)}
                  />
                )}
                {(field.kind === 'text' ||
                  field.kind === 'email' ||
                  field.kind === 'url' ||
                  field.kind === 'number') && (
                  <Input
                    id={id}
                    type={field.kind}
                    value={typeof value === 'string' ? value : ''}
                    onChange={(e) => set(field.key, e.target.value)}
                  />
                )}
                {field.kind === 'select' && (
                  <ChoiceSelect
                    label={field.label}
                    value={typeof value === 'string' ? value : ''}
                    items={field.items}
                    onChange={(v) => set(field.key, v)}
                  />
                )}
                {field.kind === 'upload' && (
                  <UploadField
                    field={field}
                    current={settings[field.key] as SiteUpload | null}
                    file={value instanceof File ? value : null}
                    onFile={(file) => set(field.key, file)}
                    onRemoved={load}
                  />
                )}
                {overridden ? (
                  <p className="text-muted-foreground text-xs">
                    The instance configuration decides this, whatever is saved here.
                  </p>
                ) : (
                  field.hint && <p className="text-muted-foreground text-xs">{field.hint}</p>
                )}
              </div>
            )
          })}
          <Button type="submit" disabled={saving}>
            Save changes
          </Button>
        </form>
      )}
    </AdminLayout>
  )
}
