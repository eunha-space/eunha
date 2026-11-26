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
import { FormSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

type Key = Exclude<keyof AdminSettings, 'overridden'>

type Field =
  | { key: Key; kind: 'text' | 'email' | 'url' | 'number'; label: string; hint?: string }
  // `shownWith`: drawn only while that switch is on.
  | { key: Key; kind: 'textarea'; label: string; hint?: string; rows?: number; shownWith?: Key }
  | { key: Key; kind: 'switch'; label: string; hint?: string }
  | { key: Key; kind: 'select'; label: string; hint?: string; items: Record<string, string> }
  | { key: Key; kind: 'upload'; label: string; hint?: string }

const FEED_ACCESS = {
  get public() { return t({ id: 'mastodon.admin.settings.feed_access.modes.public', defaultMessage: 'Everyone' }) },
  get authenticated() { return t({ id: 'mastodon.admin.settings.feed_access.modes.authenticated', defaultMessage: 'Authenticated users only' }) },
  get disabled() { return t({ id: 'mastodon.admin.settings.feed_access.modes.disabled', defaultMessage: 'Require specific user role' }) },
}

const AUDIENCES = {
  get disabled() { return t({ id: 'mastodon.admin.settings.domain_blocks.disabled', defaultMessage: 'To no one' }) },
  get users() { return t({ id: 'mastodon.admin.settings.domain_blocks.users', defaultMessage: 'To logged-in local users' }) },
  get all() { return t({ id: 'mastodon.admin.settings.domain_blocks.all', defaultMessage: 'To everyone' }) },
}

/**
 * Mastodon's settings pages, each with the keys its form posts, labelled and
 * hinted as `simple_form.en.yml` labels and hints them.
 */
const PAGES: Record<string, { title: string; preamble: string; fields: Field[] }> = {
  branding: {
    get title() { return t({ id: 'mastodon.admin.settings.branding.title', defaultMessage: 'Branding' }) },
    get preamble() { return t({ id: 'eunha.admin.settings.your_server_s_branding_differentiates_it', defaultMessage: 'Your server\'\'s branding differentiates it from other servers in the network. This information may be displayed across a variety of environments.' }) },
    fields: [
      { key: 'site_title', kind: 'text', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.site_title', defaultMessage: 'Server name' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.site_title', defaultMessage: 'How people may refer to your server besides its domain name.' }) } },
      { key: 'site_contact_username', kind: 'text', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.site_contact_username', defaultMessage: 'Contact username' }) }, get hint() { return t({ id: 'eunha.admin.settings.how_people_can_reach_you_on', defaultMessage: 'How people can reach you on the fediverse.' }) } },
      { key: 'site_contact_email', kind: 'email', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.site_contact_email', defaultMessage: 'Contact e-mail' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.site_contact_email', defaultMessage: 'How people can reach you for legal or support inquiries.' }) } },
      { key: 'site_short_description', kind: 'textarea', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.site_short_description', defaultMessage: 'Server description' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.site_short_description', defaultMessage: 'A short description to help uniquely identify your server. Who is running it, who is it for?' }) }, rows: 2 },
      { key: 'thumbnail', kind: 'upload', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.thumbnail', defaultMessage: 'Server thumbnail' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.thumbnail', defaultMessage: 'A roughly 2:1 image displayed alongside your server information.' }) } },
      { key: 'thumbnail_description', kind: 'text', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.thumbnail_description', defaultMessage: 'Thumbnail alt text' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.thumbnail_description', defaultMessage: 'A description of the image to help people with visual impairments understand its content.' }) } },
      { key: 'favicon', kind: 'upload', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.favicon', defaultMessage: 'Favicon' }) }, get hint() { return t({ id: 'eunha.admin.settings.webp_png_gif_or_jpg_overrides', defaultMessage: 'WEBP, PNG, GIF or JPG. Overrides the default favicon with a custom icon.' }) } },
      { key: 'app_icon', kind: 'upload', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.app_icon', defaultMessage: 'App icon' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.app_icon', defaultMessage: 'WEBP, PNG, GIF or JPG. Overrides the default app icon on mobile devices with a custom icon.' }) } },
      {
        key: 'landing_page',
        kind: 'select',
        get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.landing_page', defaultMessage: 'Landing page for new visitors' }) },
        items: { get trends() { return t({ id: 'explore.title', defaultMessage: 'Trending' }) }, get overview() { return t({ id: 'mastodon.admin.settings.landing_page.values.overview', defaultMessage: 'Overview' }) }, get local_feed() { return t({ id: 'mastodon.admin.settings.landing_page.values.local_feed', defaultMessage: 'Local live feed' }) }, get about() { return t({ id: 'mastodon.admin.settings.landing_page.values.about', defaultMessage: 'About page' }) } },
      },
    ],
  },
  about: {
    get title() { return t({ id: 'column.about', defaultMessage: 'About' }) },
    get preamble() { return t({ id: 'mastodon.admin.settings.about.preamble', defaultMessage: 'Provide in-depth information about how the server is operated, moderated, funded.' }) },
    fields: [
      { key: 'site_extended_description', kind: 'textarea', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.site_extended_description', defaultMessage: 'Extended description' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.site_extended_description', defaultMessage: 'Any additional information that may be useful to visitors and your users. Can be structured with Markdown syntax.' }) }, rows: 8 },
      { key: 'show_domain_blocks', kind: 'select', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.show_domain_blocks', defaultMessage: 'Show domain blocks' }) }, items: AUDIENCES },
      { key: 'show_domain_blocks_rationale', kind: 'select', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.show_domain_blocks_rationale', defaultMessage: 'Show why domains were blocked' }) }, items: AUDIENCES },
      { key: 'status_page_url', kind: 'url', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.status_page_url', defaultMessage: 'Status page URL' }) }, get hint() { return t({ id: 'eunha.admin.settings.url_of_a_page_where_people', defaultMessage: 'URL of a page where people can see the status of this server during an outage.' }) } },
      { key: 'site_terms', kind: 'textarea', get label() { return t({ id: 'privacy_policy.title', defaultMessage: 'Privacy Policy' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.site_terms', defaultMessage: 'Use your own privacy policy or leave blank to use the default. Can be structured with Markdown syntax.' }) }, rows: 8 },
    ],
  },
  registrations: {
    get title() { return t({ id: 'mastodon.admin.settings.registrations.title', defaultMessage: 'Registrations' }) },
    get preamble() { return t({ id: 'mastodon.admin.settings.registrations.preamble', defaultMessage: 'Control who can create an account on your server.' }) },
    fields: [
      { key: 'min_age', kind: 'number', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.min_age', defaultMessage: 'Minimum age requirement' }) }, get hint() { return t({ id: 'eunha.admin.settings.users_will_be_asked_to_confirm', defaultMessage: 'Users will be asked to confirm their date of birth during sign-up.' }) } },
      {
        key: 'registrations_mode',
        kind: 'select',
        get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.registrations_mode', defaultMessage: 'Who can sign-up' }) },
        get hint() { return t({ id: 'mastodon.admin.settings.registrations_mode.warning_hint', defaultMessage: 'We recommend using “Approval required for sign up” unless you are confident your moderation team can handle spam and malicious registrations in a timely fashion.' }) },
        items: { get open() { return t({ id: 'mastodon.admin.settings.registrations_mode.modes.open', defaultMessage: 'Anyone can sign up' }) }, get approved() { return t({ id: 'mastodon.admin.settings.registrations_mode.modes.approved', defaultMessage: 'Approval required for sign up' }) }, get none() { return t({ id: 'mastodon.admin.settings.registrations_mode.modes.none', defaultMessage: 'Nobody can sign up' }) } },
      },
      { key: 'require_invite_text', kind: 'switch', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.require_invite_text', defaultMessage: 'Require a reason to join' }) }, get hint() { return t({ id: 'eunha.admin.settings.when_sign_ups_require_manual_approval', defaultMessage: 'When sign-ups require manual approval, make the “Why do you want to join?” text input mandatory rather than optional.' }) } },
      { key: 'welcome_email', kind: 'switch', get label() { return t({ id: 'eunha.admin.settings.send_new_users_a_welcome_email', defaultMessage: 'Send new users a welcome email' }) }, get hint() { return t({ id: 'eunha.admin.settings.an_hour_after_an_account_is', defaultMessage: 'An hour after an account is ready, email its owner a getting-started checklist, accounts to follow and trending hashtags.' }) } },
      { key: 'captcha_enabled', kind: 'switch', get label() { return t({ id: 'eunha.admin.settings.require_new_users_to_solve_a', defaultMessage: 'Require new users to solve a CAPTCHA' }) }, get hint() { return t({ id: 'eunha.admin.settings.saved_for_a_mastodon_on_the', defaultMessage: 'Saved for a Mastodon on the same database; eunha has no CAPTCHA to show.' }) } },
      { key: 'closed_registrations_message', kind: 'textarea', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.closed_registrations_message', defaultMessage: 'Custom message when sign-ups are not available' }) }, get hint() { return t({ id: 'eunha.admin.settings.displayed_when_sign_ups_are_closed', defaultMessage: 'Displayed when sign-ups are closed.' }) }, rows: 3 },
    ],
  },
  discovery: {
    get title() { return t({ id: 'mastodon.admin.settings.discovery.title', defaultMessage: 'Discovery' }) },
    get preamble() { return t({ id: 'eunha.admin.settings.surfacing_interesting_content_is_instrumental_in', defaultMessage: 'Surfacing interesting content is instrumental in onboarding new users who may not know anyone. Control how various discovery features work on your server.' }) },
    fields: [
      { key: 'trends', kind: 'switch', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.trends', defaultMessage: 'Enable trends' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.trends', defaultMessage: 'Trends show which posts, hashtags and news stories are gaining traction on your server.' }) } },
      { key: 'trendable_by_default', kind: 'switch', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.trendable_by_default', defaultMessage: 'Allow trends without prior review' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.trendable_by_default', defaultMessage: 'Skip manual review of trending content. Individual items can still be removed from trends after the fact.' }) } },
      { key: 'local_live_feed_access', kind: 'select', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.local_live_feed_access', defaultMessage: 'Access to live feeds featuring local posts' }) }, items: FEED_ACCESS },
      { key: 'remote_live_feed_access', kind: 'select', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.remote_live_feed_access', defaultMessage: 'Access to live feeds featuring remote posts' }) }, items: FEED_ACCESS },
      {
        key: 'local_topic_feed_access',
        kind: 'select',
        get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.local_topic_feed_access', defaultMessage: 'Access to hashtag and link feeds featuring local posts' }) },
        items: { public: FEED_ACCESS.public, authenticated: FEED_ACCESS.authenticated },
      },
      { key: 'remote_topic_feed_access', kind: 'select', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.remote_topic_feed_access', defaultMessage: 'Access to hashtag and link feeds featuring remote posts' }) }, items: FEED_ACCESS },
      { key: 'allow_referrer_origin', kind: 'switch', get label() { return t({ id: 'eunha.admin.settings.allow_external_websites_to_see_your', defaultMessage: 'Allow external websites to see your server as the source of traffic' }) } },
      { key: 'activity_api_enabled', kind: 'switch', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.activity_api_enabled', defaultMessage: 'Publish aggregate statistics about user activity in the API' }) }, get hint() { return t({ id: 'eunha.admin.settings.counts_of_locally_published_posts_active', defaultMessage: 'Counts of locally published posts, active users, and new registrations in weekly buckets.' }) } },
      { key: 'peers_api_enabled', kind: 'switch', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.peers_api_enabled', defaultMessage: 'Publish list of discovered servers in the API' }) }, get hint() { return t({ id: 'eunha.admin.settings.a_list_of_domain_names_this', defaultMessage: 'A list of domain names this server has encountered in the fediverse.' }) } },
      {
        key: 'authorized_fetch',
        kind: 'select',
        get label() { return t({ id: 'mastodon.admin.settings.security.authorized_fetch', defaultMessage: 'Require authentication from federated servers' }) },
        get hint() { return t({ id: 'eunha.admin.settings.which_activitypub_requests_have_to_be', defaultMessage: 'Which ActivityPub requests have to be signed. Requiring it of profiles lets the server tell who an account is known to.' }) },
        items: { get none() { return t({ id: 'mastodon.admin.reports.comment.none', defaultMessage: 'None' }) }, get actors() { return t({ id: 'search_results.accounts', defaultMessage: 'Profiles' }) }, get all() { return t({ id: 'notifications.filter.all', defaultMessage: 'All' }) } },
      },
      { key: 'bootstrap_timeline_accounts', kind: 'text', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.bootstrap_timeline_accounts', defaultMessage: 'Always recommend these accounts to new users' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.bootstrap_timeline_accounts', defaultMessage: 'These accounts will be pinned to the top of new users\'\' follow recommendations. Provide a comma-separated list of accounts.' }) } },
      { key: 'opt_in_server_directory', kind: 'switch', get label() { return t({ id: 'eunha.admin.settings.list_this_server_in_server_directories', defaultMessage: 'List this server in server directories' }) }, get hint() { return t({ id: 'eunha.admin.settings.tells_other_services_this_server_wants', defaultMessage: 'Tells other services this server wants to be listed.' }) } },
      { key: 'opt_in_server_recommendation', kind: 'switch', get label() { return t({ id: 'eunha.admin.settings.allow_this_server_to_be_recommended', defaultMessage: 'Allow this server to be recommended' }) }, get hint() { return t({ id: 'eunha.admin.settings.tells_other_services_this_server_may', defaultMessage: 'Tells other services this server may be recommended to people.' }) } },
      { key: 'profile_directory', kind: 'switch', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.profile_directory', defaultMessage: 'Enable profile directory' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.profile_directory', defaultMessage: 'The profile directory lists all users who have opted-in to be discoverable.' }) } },
      { key: 'wrapstodon', kind: 'switch', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.wrapstodon', defaultMessage: 'Enable Wrapstodon' }) }, get hint() { return t({ id: 'eunha.admin.settings.offer_local_users_to_generate_a', defaultMessage: 'Offer local users to generate a playful summary of their use during the year.' }) } },
    ],
  },
  crawlers: {
    get title() { return t({ id: 'mastodon.admin.settings.crawlers.title', defaultMessage: 'Crawler controls' }) },
    get preamble() { return t({ id: 'mastodon.admin.settings.crawlers.preamble', defaultMessage: 'Control how (well-behaved) bots such as search engine crawlers can interact with your server. Malicious bots can ignore these settings and must be blocked at the network level.' }) },
    fields: [
      { key: 'override_robots', kind: 'switch', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.override_robots', defaultMessage: 'Override default robots.txt' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.override_robots', defaultMessage: 'Allows you to customize rules of access for well-behaved bots' }) } },
      { key: 'custom_robots', kind: 'textarea', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.custom_robots', defaultMessage: 'Custom robots.txt' }) }, get hint() { return t({ id: 'eunha.admin.settings.robots_txt_syntax', defaultMessage: 'Written as RFC 9309 describes. It replaces the default rules, which keep AI crawlers out.' }) }, rows: 8, shownWith: 'override_robots' },
      { key: 'noindex', kind: 'switch', get label() { return t({ id: 'mastodon.admin.settings.default_noindex.title', defaultMessage: 'Opt users out of search engine indexing by default' }) }, get hint() { return t({ id: 'eunha.admin.settings.affects_all_users_who_have_not', defaultMessage: 'Affects all users who have not changed this setting themselves.' }) } },
    ],
  },
  content_retention: {
    get title() { return t({ id: 'mastodon.admin.settings.content_retention.title', defaultMessage: 'Content retention' }) },
    get preamble() { return t({ id: 'eunha.admin.settings.control_how_user_generated_content_is', defaultMessage: 'Control how user-generated content is stored on this server.' }) },
    fields: [
      { key: 'media_cache_retention_period', kind: 'number', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.media_cache_retention_period', defaultMessage: 'Media cache retention period' }) }, get hint() { return t({ id: 'eunha.admin.settings.days_media_from_remote_posts_and', defaultMessage: 'Days. Media from remote posts and link preview images are forgotten after this many days; blank keeps them.' }) } },
      { key: 'content_cache_retention_period', kind: 'number', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.content_cache_retention_period', defaultMessage: 'Remote content retention period' }) }, get hint() { return t({ id: 'eunha.admin.settings.days_all_posts_from_other_servers', defaultMessage: 'Days. All posts from other servers, boosts and replies included, are deleted after this many days, whatever local users did with them. Intended for special purpose servers.' }) } },
      { key: 'backups_retention_period', kind: 'number', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.backups_retention_period', defaultMessage: 'User archive retention period' }) }, get hint() { return t({ id: 'eunha.admin.settings.days_after_which_archives_users_generated', defaultMessage: 'Days after which archives users generated are deleted.' }) } },
    ],
  },
  appearance: {
    get title() { return t({ id: 'mastodon.admin.settings.appearance.title', defaultMessage: 'Appearance' }) },
    get preamble() { return t({ id: 'eunha.admin.settings.customize_the_web_interface', defaultMessage: 'Customize the web interface.' }) },
    fields: [
      { key: 'theme', kind: 'select', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.theme', defaultMessage: 'Default theme' }) }, items: { get default() { return t({ id: 'about.default_locale', defaultMessage: 'Default' }) } } },
      { key: 'custom_css', kind: 'textarea', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.custom_css', defaultMessage: 'Custom CSS' }) }, get hint() { return t({ id: 'eunha.admin.settings.you_can_apply_custom_styles_on', defaultMessage: 'You can apply custom styles on the web version of this server.' }) }, rows: 10 },
      { key: 'mascot', kind: 'upload', get label() { return t({ id: 'mastodon.simple_form.labels.form_admin_settings.mascot', defaultMessage: 'Custom mascot (legacy)' }) }, get hint() { return t({ id: 'mastodon.simple_form.hints.form_admin_settings.mascot', defaultMessage: 'Overrides the illustration in the advanced web interface.' }) } },
    ],
  },
}

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
            title={t({ id: 'eunha.admin.settings.remove_image', defaultMessage: 'Remove the {field}?' }, { field: field.label.toLowerCase() })}
            description={t({ id: 'eunha.admin.settings.the_default_is_shown_again', defaultMessage: 'The default is shown again.' })}
            confirmLabel={t({ id: 'tag.remove', defaultMessage: 'Remove' })}
            onConfirm={async () => {
              await deleteSiteUpload(token ?? '', current.id)
              toast.success(t({ id: 'eunha.common.removed', defaultMessage: 'Removed.' }))
              onRemoved()
            }}
          >
            {t({ id: 'tag.remove', defaultMessage: 'Remove' })}
          </ConfirmButton>
        </div>
      )}
      <Input
        id={`setting-${field.key}`}
        type="file"
        accept="image/jpeg,image/png,image/gif,image/webp"
        onChange={(e) => onFile(e.target.files?.[0] ?? null)}
      />
      {file && <p className="text-muted-foreground text-xs">{t({ id: 'eunha.admin.settings.uploaded_on_save', defaultMessage: '{name} will be uploaded on save.' }, { name: file.name })}</p>}
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
      toast.success(t({ id: 'mastodon.generic.changes_saved_msg', defaultMessage: 'Changes successfully saved!' }))
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  const set = (key: Key, value: string | boolean | File | null) =>
    setDraft((d) => ({ ...d, [key]: value }))

  return (
    <AdminLayout title={t({ id: 'eunha.admin.settings.server_settings', defaultMessage: 'Server settings · {title}' }, { title: def.title })} permission="manage_settings">
      <AdminError error={error} />
      {settings === null && !error && <FormSkeleton fields={5} />}
      {settings && (
        <form onSubmit={save} className="max-w-2xl space-y-5">
          <p className="text-muted-foreground text-sm">{def.preamble}</p>
          {def.fields.map((field) => {
            if (field.kind === 'textarea' && field.shownWith && draft[field.shownWith] !== true) return null
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
                    {t({ id: 'eunha.admin.settings.the_instance_configuration_decides_this_whatever', defaultMessage: 'The instance configuration decides this, whatever is saved here.' })}
                  </p>
                ) : (
                  field.hint && <p className="text-muted-foreground text-xs">{field.hint}</p>
                )}
              </div>
            )
          })}
          <Button type="submit" disabled={saving}>
            {t({ id: 'subscribed_languages.save', defaultMessage: 'Save changes' })}
          </Button>
        </form>
      )}
    </AdminLayout>
  )
}
