import type { mastodon } from '../masto.ts'

/**
 * Drop direct messages from a list of statuses.
 *
 * Mastodon 5.0 takes messages out of the timeline and out of profiles
 * entirely — they live on the Messages page, where a thread has somewhere to
 * belong, and notifications are unaffected. The server still sends them, so
 * the client is where it happens.
 *
 * Filtered where the list is rendered rather than where it is fetched, on
 * purpose. `useInfiniteFeed` pages from the last item it holds and stops when
 * a page comes back empty, so removing statuses from a page before the feed
 * sees it would end a timeline early for anyone whose next page happened to be
 * all messages. Filtering here leaves the cursor on what the server actually
 * returned.
 */
export function withoutMessages(
  statuses: mastodon.v1.Status[] | null,
): mastodon.v1.Status[] | null {
  if (!statuses) return statuses
  // A boost carries its own visibility, so the underlying status is the one to
  // ask — though a direct status cannot be boosted in the first place.
  return statuses.filter((s) => (s.reblog ?? s).visibility !== 'direct')
}

/** What the Quote item of a post's menu says, and whether it can be chosen. */
export interface QuoteItemState {
  title: string
  // The line under the title saying why, or what follows.
  meta?: string
  disabled: boolean
}

/**
 * Mastodon's `quoteItemState` (`boost_button_utils.ts`), over the conditions
 * `selectStatusConditions` works out: a post can be quoted only when the
 * server says the viewer's quote would be accepted, automatically or by hand,
 * and it is public or unlisted. Anything else — `denied`, `unknown`, or no
 * policy at all — leaves the item there but disabled, saying why.
 */
export function quoteItemState(
  status: mastodon.v1.Status,
  meId: string | null,
): QuoteItemState {
  const isLoggedIn = !!meId
  const isPublic = status.visibility === 'public' || status.visibility === 'unlisted'
  const isMine = isLoggedIn && status.account.id === meId
  // Absent when the post came over a public or hashtag stream, whose payload
  // was written for no one in particular (see `useStatusStreaming`).
  const approval = status.quoteApproval as mastodon.v1.QuoteApproval | null | undefined
  const currentUser = approval?.currentUser
  const isFollowersOnly =
    approval?.automatic[0] === 'followers' || approval?.manual[0] === 'followers'

  if (!isPublic && !isMine) {
    return { title: 'Quote', meta: 'Private posts cannot be quoted', disabled: true }
  }
  if ((currentUser === 'automatic' || currentUser === 'manual') && isPublic) {
    return { title: 'Quote', disabled: false }
  }
  if (isLoggedIn) {
    return {
      title: 'Quote',
      meta: isFollowersOnly
        ? 'Only followers can quote this post'
        : 'You are not allowed to quote this post',
      disabled: true,
    }
  }
  return { title: 'Quote', disabled: false }
}

/**
 * Mastodon's `quoteComposeByStatus` checks this before it opens a quote, and
 * says "You are not authorized to quote this post." when it fails.
 */
export function mayQuote(status: mastodon.v1.Status): boolean {
  const currentUser = (status.quoteApproval as mastodon.v1.QuoteApproval | null | undefined)
    ?.currentUser
  return currentUser === 'automatic' || currentUser === 'manual'
}
