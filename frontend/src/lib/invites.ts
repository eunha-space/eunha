import type { Invite } from '../eunha-api.ts'

export function inviteDate(value: string): Date {
  return new Date(/[Z+]|-\d\d:\d\d$/.test(value) ? value : `${value}Z`)
}
export function inviteStatus(
  invite: Invite,
): 'Available' | 'Used' | 'Expired' | 'Unavailable' {
  if (invite.max_uses !== null && invite.uses >= invite.max_uses) return 'Used'
  if (
    invite.expired ||
    (invite.expires_at && inviteDate(invite.expires_at).getTime() <= Date.now())
  )
    return 'Expired'
  return invite.valid_for_use ? 'Available' : 'Unavailable'
}
export function inviteExpiry(invite: Invite): string {
  if (!invite.expires_at) return 'Never expires'
  return `${inviteStatus(invite) === 'Expired' ? 'Expired' : 'Expires'} ${inviteDate(invite.expires_at).toLocaleString()}`
}
/** A link count is meaningful as a people allowance only for single-use links. */
export function availableInviteCount(invites: Invite[]): number | null {
  const usable = invites.filter((i) => inviteStatus(i) === 'Available')
  return usable.length && usable.every((i) => i.max_uses === 1)
    ? usable.length
    : null
}

/**
 * The code in what someone typed or pasted: a bare code, or an invite link in
 * either Mastodon's form (`/invite/CODE`) or the sign-up page's (`?invite=`).
 */
export function inviteCodeFrom(input: string): string {
  const text = input.trim()
  if (!/^https?:\/\/|^\/|\?invite=|\/invite\//.test(text)) return text
  try {
    const url = new URL(text, window.location.origin)
    const query = url.searchParams.get('invite') ?? url.searchParams.get('invite_code')
    if (query) return query.trim()
    const path = url.pathname.match(/\/invite\/([^/]+)\/?$/)
    if (path) return decodeURIComponent(path[1]).replace(/\.json$/, '')
  } catch {
    // Not a link after all.
  }
  return text
}
