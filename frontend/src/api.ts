// Mastodon C2S API calls, backed by masto.js. The same API is consumed by
// third-party mobile apps; this frontend is just one client.
import { restClient, type mastodon } from './masto.ts'

let cachedInstance: mastodon.v2.Instance | null = null

// Keep branding available synchronously when navigation remounts the sidebar.
export function getCachedInstance(): mastodon.v2.Instance | null {
  return cachedInstance
}

export async function getInstance(): Promise<mastodon.v2.Instance> {
  const instance = await restClient().v2.instance.fetch()
  cachedInstance = instance
  return instance
}

// The instance's own policy documents. masto types neither, and both are plain
// public GETs, so they are fetched directly. `content` is HTML the server
// rendered from Markdown with HTML escaped (Mastodon's Redcarpet settings).

/** `REST::TermsOfServiceSerializer`. */
export interface TermsOfService {
  /** `YYYY-MM-DD`. */
  effective_date: string
  effective: boolean
  content: string
  succeeded_by: string | null
}

/** `REST::PrivacyPolicySerializer`. */
export interface PrivacyPolicy {
  updated_at: string
  content: string
}

/**
 * The current terms, or with `date` the version effective then; `null` when
 * there are none (the server answers 404).
 */
export async function getTermsOfService(date?: string): Promise<TermsOfService | null> {
  const path = date
    ? `/api/v1/instance/terms_of_service/${encodeURIComponent(date)}`
    : '/api/v1/instance/terms_of_service'
  const res = await fetch(`${window.location.origin}${path}`)
  if (res.status === 404) return null
  if (!res.ok) throw new Error(`GET ${path} failed: ${res.status}`)
  return (await res.json()) as TermsOfService
}

export async function getPrivacyPolicy(): Promise<PrivacyPolicy> {
  const res = await fetch(`${window.location.origin}/api/v1/instance/privacy_policy`)
  if (!res.ok) throw new Error(`GET /api/v1/instance/privacy_policy failed: ${res.status}`)
  return (await res.json()) as PrivacyPolicy
}

// The terms of service interstitial: terms a notification flagged this user to
// be shown, until they open the terms page (eunha's stand-in for the page
// Mastodon's web app renders; the `terms-of-service-interstitial-api`
// divergence).
const INTERSTITIAL = '/api/eunha/v1/terms_of_service/interstitial'

export async function getTermsOfServiceInterstitial(
  token: string,
): Promise<TermsOfService | null> {
  const res = await fetch(`${window.location.origin}${INTERSTITIAL}`, {
    headers: { Authorization: `Bearer ${token}` },
  })
  if (!res.ok) return null
  const body = (await res.json()) as { terms_of_service: TermsOfService | null }
  return body.terms_of_service
}

export async function dismissTermsOfServiceInterstitial(token: string): Promise<void> {
  await fetch(`${window.location.origin}${INTERSTITIAL}`, {
    method: 'DELETE',
    headers: { Authorization: `Bearer ${token}` },
  })
}

export async function getHomeTimeline(
  token: string,
  maxId?: string,
): Promise<mastodon.v1.Status[]> {
  // The paginator is awaitable and resolves to the first page.
  return restClient(token).v1.timelines.home.list({ limit: 40, maxId })
}

// masto declares `QuoteApprovalPolicy` twice: the entity's — public,
// followers, following, unsupported_policy — and the create parameter's, which
// drops the last two and adds `nobody`. `mastodon.v1.QuoteApprovalPolicy`
// resolves to the entity one, so naming it here would be the wrong union by a
// value that matters. This is the create parameter's set, spelled out.
export type QuotePolicy = 'public' | 'followers' | 'nobody'

export function postStatus(
  token: string,
  params: {
    status: string
    visibility?: mastodon.v1.StatusVisibility
    inReplyToId?: string
    quotedStatusId?: string
    mediaIds?: string[]
    quoteApprovalPolicy?: QuotePolicy
  },
): Promise<mastodon.v1.Status> {
  const { status, visibility, inReplyToId, quotedStatusId, mediaIds, quoteApprovalPolicy } =
    params
  // With media, masto requires the media-ids variant (status optional).
  if (mediaIds && mediaIds.length > 0) {
    return restClient(token).v1.statuses.create({
      status,
      visibility,
      inReplyToId,
      quotedStatusId,
      mediaIds,
      quoteApprovalPolicy,
    })
  }
  return restClient(token).v1.statuses.create({
    quoteApprovalPolicy,
    status,
    visibility,
    inReplyToId,
    quotedStatusId,
  })
}

export function uploadMedia(
  file: File,
  token: string,
  description?: string,
): Promise<mastodon.v1.MediaAttachment> {
  return restClient(token).v2.media.create({ file, description })
}

export function updateMediaDescription(
  id: string,
  description: string,
  token: string,
): Promise<mastodon.v1.MediaAttachment> {
  return restClient(token).v1.media.$select(id).update({ description })
}

// Status interactions. Each returns the updated status; the caller normalizes
// reblog wrappers (a boost response wraps the original in `.reblog`).
export function setFavourite(token: string, id: string, on: boolean) {
  const s = restClient(token).v1.statuses.$select(id)
  return on ? s.favourite() : s.unfavourite()
}

export function setReblog(token: string, id: string, on: boolean) {
  const s = restClient(token).v1.statuses.$select(id)
  return on ? s.reblog() : s.unreblog()
}

export function setBookmark(token: string, id: string, on: boolean) {
  const s = restClient(token).v1.statuses.$select(id)
  return on ? s.bookmark() : s.unbookmark()
}

export function getCurrentAccount(token: string): Promise<mastodon.v1.AccountCredentials> {
  return restClient(token).v1.accounts.verifyCredentials()
}

/** Mastodon `UserRole::FLAGS`, the two an invite page turns on. */
const INVITE_USERS = 1 << 16
const MANAGE_INVITES = 1 << 11

export interface InvitePermissions {
  /** May create invites of their own. */
  canInvite: boolean
  /** May hand invites out to other members, and revoke anyone's. */
  canGrant: boolean
}

/**
 * What the signed-in account may do with invites.
 *
 * `verify_credentials` carries the role's *computed* permissions — the account's
 * own role unioned with the everyone role's, which is where `invite_users` sits
 * by default — so this reads the same set the server authorizes against. An
 * instance that would rather hand invites out itself clears that bit, and
 * `canInvite` turns false for everyone but its staff.
 */
export async function getInvitePermissions(
  token: string,
): Promise<InvitePermissions> {
  const me = await getCurrentAccount(token)
  // masto.js does not model `role` on the credential account.
  const { role } = me as unknown as { role?: { permissions?: string } }
  const permissions = Number(role?.permissions ?? 0)
  return {
    canInvite: (permissions & INVITE_USERS) !== 0,
    canGrant: (permissions & MANAGE_INVITES) !== 0,
  }
}

export function updateAccountImages(
  token: string,
  params: { avatar?: File; header?: File },
): Promise<mastodon.v1.AccountCredentials> {
  return restClient(token).v1.accounts.updateCredentials(params)
}

export function updateAccountProfile(
  token: string,
  params: {
    displayName?: string
    note?: string
    fieldsAttributes?: { name: string; value: string }[]
  },
): Promise<mastodon.v1.AccountCredentials> {
  return restClient(token).v1.accounts.updateCredentials(params)
}

export async function getFeaturedTags(
  token: string,
): Promise<mastodon.v1.FeaturedTag[]> {
  return restClient(token).v1.featuredTags.list()
}

export async function getFeaturedTagSuggestions(
  token: string,
): Promise<mastodon.v1.Tag[]> {
  return restClient(token).v1.featuredTags.suggestions.list()
}

export function createFeaturedTag(
  token: string,
  name: string,
): Promise<mastodon.v1.FeaturedTag> {
  return restClient(token).v1.featuredTags.create({ name })
}

export function deleteFeaturedTag(token: string, id: string): Promise<void> {
  return restClient(token).v1.featuredTags.$select(id).remove()
}

export function deleteProfileAvatar(token: string): Promise<mastodon.v1.Account> {
  return restClient(token).v1.profile.avatar.remove()
}

export function deleteProfileHeader(token: string): Promise<mastodon.v1.Account> {
  return restClient(token).v1.profile.header.remove()
}

export function lookupAccount(
  acct: string,
  token?: string,
): Promise<mastodon.v1.Account> {
  return restClient(token).v1.accounts.lookup({ acct })
}

export async function getAccountStatuses(
  id: string,
  token?: string,
  maxId?: string,
): Promise<mastodon.v1.Status[]> {
  return restClient(token).v1.accounts.$select(id).statuses.list({ limit: 40, maxId })
}

// Report an account, optionally naming posts of theirs and the rules they
// break. `rule_ids` only means something with the `violation` category, and
// `forward_to_domains` only for a remote account: the servers, beyond the
// account's own, whose posts the report names and that should hear of it too.
export function fileReport(
  token: string,
  params: {
    accountId: string
    statusIds?: string[]
    comment?: string
    forward?: boolean
    forwardToDomains?: string[]
    category?: mastodon.v1.ReportCategory
    ruleIds?: string[]
  },
): Promise<mastodon.v1.Report> {
  return restClient(token).v1.reports.create(params)
}

export interface InstanceRule {
  id: string
  text: string
  hint?: string
}

// The server's rules. Public, and empty on a server that has written none —
// which is how the report flow knows to leave the rule step out.
export async function getInstanceRules(): Promise<InstanceRule[]> {
  const res = await fetch(`${window.location.origin}/api/v1/instance/rules`)
  if (!res.ok) return []
  const body: unknown = await res.json()
  return Array.isArray(body) ? (body as InstanceRule[]) : []
}

/**
 * `REST::TranslationSerializer`. masto's `Translation` type predates most of
 * these fields, so this is fetched directly.
 */
export interface StatusTranslation {
  content: string
  spoiler_text: string
  detected_source_language: string | null
  language: string
  provider: string | null
  poll: { id: string; options: { title: string }[] } | null
  media_attachments: { id: string; description: string }[]
}

/**
 * Which languages each source language translates into, with `und` for a
 * post whose language is unknown; `{}` when the server translates nothing.
 */
export type TranslationLanguages = Record<string, string[]>

let translationLanguages: Promise<TranslationLanguages> | null = null

// Asked once per page load, as Mastodon's web client asks once at start-up,
// and only when `configuration.translation.enabled` says there is anything
// to ask about.
export function getTranslationLanguages(): Promise<TranslationLanguages> {
  translationLanguages ??= (async () => {
    try {
      const instance = await getInstance()
      if (!instance.configuration.translation.enabled) return {}
      const res = await fetch(
        `${window.location.origin}/api/v1/instance/translation_languages`,
      )
      return res.ok ? ((await res.json()) as TranslationLanguages) : {}
    } catch {
      return {}
    }
  })()
  return translationLanguages
}

export async function translateStatus(
  id: string,
  token: string,
  lang: string,
): Promise<StatusTranslation> {
  const res = await fetch(
    `${window.location.origin}/api/v1/statuses/${id}/translate`,
    {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${token}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({ lang }),
    },
  )
  const body: unknown = await res.json().catch(() => null)
  if (!res.ok) {
    const message =
      body && typeof body === 'object' && 'error' in body
        ? String((body as { error: unknown }).error)
        : `Translation failed (${res.status})`
    throw new Error(message)
  }
  return body as StatusTranslation
}

// An account's pinned posts. eunha also serves `/api/v1/accounts/:id/pins`, but
// `?pinned=true` is the form Mastodon documents and masto types, and the one
// every other client already asks for. It returns all of them at once — pins
// cap at five — so there is nothing to paginate.
export async function getPinnedStatuses(
  id: string,
  token?: string,
): Promise<mastodon.v1.Status[]> {
  return restClient(token).v1.accounts.$select(id).statuses.list({ pinned: true })
}

// Pinning is capped at five by the server, and it refuses a boost or someone
// else's post — the caller surfaces what it says rather than guessing here.
export function setPin(id: string, token: string, on: boolean) {
  const s = restClient(token).v1.statuses.$select(id)
  return on ? s.pin() : s.unpin()
}

export function getStatus(id: string, token?: string): Promise<mastodon.v1.Status> {
  return restClient(token).v1.statuses.$select(id).fetch()
}

export function getStatusContext(
  id: string,
  token?: string,
): Promise<mastodon.v1.Context> {
  return restClient(token).v1.statuses.$select(id).context.fetch()
}

export function deleteStatus(id: string, token: string): Promise<mastodon.v1.Status> {
  return restClient(token).v1.statuses.$select(id).remove()
}

// Every known version of a status, oldest first, with the current one appended
// by the server. Unlike the other lists here this one does not paginate — the
// handler returns the whole history in one response and emits no `Link` header
// — so awaiting the paginator for its first (and only) page is the whole call.
export async function getStatusHistory(
  id: string,
  token?: string,
): Promise<mastodon.v1.StatusEdit[]> {
  return restClient(token).v1.statuses.$select(id).history.list()
}

export function getStatusSource(
  id: string,
  token: string,
): Promise<mastodon.v1.StatusSource> {
  return restClient(token).v1.statuses.$select(id).source.fetch()
}

export function updateStatus(
  id: string,
  params: { status: string; spoilerText?: string },
  token: string,
): Promise<mastodon.v1.Status> {
  return restClient(token).v1.statuses.$select(id).update(params)
}

export async function getRelationship(
  id: string,
  token: string,
): Promise<mastodon.v1.Relationship | undefined> {
  const rels = await restClient(token).v1.accounts.relationships.fetch({ id: [id] })
  return rels[0]
}

export function setFollow(
  id: string,
  token: string,
  on: boolean,
  params?: { reblogs?: boolean | null },
) {
  const a = restClient(token).v1.accounts.$select(id)
  return on ? a.follow(params) : a.unfollow(params)
}

export function setMute(
  id: string,
  token: string,
  on: boolean,
  params?: { notifications?: boolean; duration?: number },
) {
  const a = restClient(token).v1.accounts.$select(id)
  return on ? a.mute(params) : a.unmute()
}

// Blocking severs follows in both directions and drops pending requests, which
// unblocking does not put back — the confirmation before this says so.
export function setBlock(id: string, token: string, on: boolean) {
  const a = restClient(token).v1.accounts.$select(id)
  return on ? a.block() : a.unblock()
}

// Blocks, mutes and bookmarks all paginate by the id of the block, mute or
// bookmark row rather than by the account or status they return — the same
// reason `getFavouritedBy` hands back a paginator. See `useInfinitePaginator`.
export function getBlocks(token: string): mastodon.Paginator<mastodon.v1.Account[]> {
  return restClient(token).v1.blocks.list()
}

export function getMutes(token: string): mastodon.Paginator<mastodon.v1.Account[]> {
  return restClient(token).v1.mutes.list()
}

export function getBookmarks(token: string): mastodon.Paginator<mastodon.v1.Status[]> {
  return restClient(token).v1.bookmarks.list()
}

// Trends paginate by `offset` rather than by a cursor, and the server only
// emits a `next` link while a page came back full — so these walk the paginator
// too, and it stops on its own at the first short page.
export function getTrendingStatuses(
  token?: string,
): mastodon.Paginator<mastodon.v1.Status[]> {
  return restClient(token).v1.trends.statuses.list()
}

export function getTrendingTags(token?: string): mastodon.Paginator<mastodon.v1.Tag[]> {
  return restClient(token).v1.trends.tags.list()
}

export function getTrendingLinks(
  token?: string,
): mastodon.Paginator<mastodon.v1.TrendLink[]> {
  return restClient(token).v1.trends.links.list()
}

export async function getPublicTimeline(
  local: boolean,
  token?: string,
  maxId?: string,
): Promise<mastodon.v1.Status[]> {
  return restClient(token).v1.timelines.public.list({ local, limit: 40, maxId })
}

// Direct-message threads. Paginated by the id of each conversation's last
// status rather than by the conversation's own id, so this hands back the
// paginator like the other `Link`-header lists.
export function getConversations(
  token: string,
): mastodon.Paginator<mastodon.v1.Conversation[]> {
  return restClient(token).v1.conversations.list()
}

export function markConversationRead(token: string, id: string) {
  return restClient(token).v1.conversations.$select(id).read()
}

export function deleteConversation(token: string, id: string) {
  return restClient(token).v1.conversations.$select(id).remove()
}

export async function getNotifications(
  token: string,
  maxId?: string,
): Promise<mastodon.v1.Notification[]> {
  return restClient(token).v1.notifications.list({ limit: 40, maxId })
}

// How many notifications have arrived since the reader last marked the
// timeline. The server caps the count, so this is "how many, up to a point"
// rather than a total — which is all a badge needs.
export async function getNotificationsUnreadCount(token: string): Promise<number> {
  const { count } = await restClient(token).v1.notifications.unreadCount.fetch()
  return count
}

// Marking the notification timeline read is what makes the badge clear. Without
// it the count is measured from a marker nobody ever moves, so it only grows.
export function markNotificationsRead(token: string, lastReadId: string) {
  return restClient(token).v1.markers.create({ notifications: { lastReadId } })
}

export function votePoll(
  pollId: string,
  choices: number[],
  token: string,
): Promise<mastodon.v1.Poll> {
  return restClient(token).v1.polls.$select(pollId).votes.create({ choices })
}

export async function search(
  q: string,
  token?: string,
): Promise<mastodon.v2.Search> {
  // resolve remote accounts/statuses only for authenticated searches.
  return restClient(token).v2.search.list({ q, resolve: !!token, limit: 20 })
}

export async function getTagTimeline(
  name: string,
  token?: string,
  maxId?: string,
): Promise<mastodon.v1.Status[]> {
  return restClient(token).v1.timelines.tag.$select(name).list({ limit: 40, maxId })
}

export async function searchAccounts(
  q: string,
  token: string,
  limit = 6,
): Promise<mastodon.v1.Account[]> {
  // Mention autocomplete: no WebFinger resolution (fast, local/known accounts).
  return restClient(token).v1.accounts.search.list({ q, limit, resolve: false })
}

export function getFollowRequests(
  token: string,
): mastodon.Paginator<mastodon.v1.Account[]> {
  return restClient(token).v1.followRequests.list({ limit: 40 })
}

export function authorizeFollowRequest(
  id: string,
  token: string,
): Promise<mastodon.v1.Relationship> {
  return restClient(token).v1.followRequests.$select(id).authorize()
}

export function rejectFollowRequest(
  id: string,
  token: string,
): Promise<mastodon.v1.Relationship> {
  return restClient(token).v1.followRequests.$select(id).reject()
}

export function getFollowers(
  id: string,
  token?: string,
): mastodon.Paginator<mastodon.v1.Account[]> {
  return restClient(token).v1.accounts.$select(id).followers.list({ limit: 40 })
}

export function getFollowing(
  id: string,
  token?: string,
): mastodon.Paginator<mastodon.v1.Account[]> {
  return restClient(token).v1.accounts.$select(id).following.list({ limit: 40 })
}

// Who favourited / boosted a status. Unlike the account lists above, these
// paginate by favourite id and reblog id — cursors the client never sees, since
// they belong to rows the response body doesn't carry. The cursor lives only in
// the response's `Link` header, so these hand back masto's paginator (which
// follows that header) instead of a page, and callers walk it with
// `useInfinitePaginator` rather than `useInfiniteFeed`.
export function getFavouritedBy(
  id: string,
  token?: string,
): mastodon.Paginator<mastodon.v1.Account[]> {
  return restClient(token).v1.statuses.$select(id).favouritedBy.list()
}

export function getRebloggedBy(
  id: string,
  token?: string,
): mastodon.Paginator<mastodon.v1.Account[]> {
  return restClient(token).v1.statuses.$select(id).rebloggedBy.list()
}
