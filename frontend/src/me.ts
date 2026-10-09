// Caches the signed-in account so UI can synchronously render current-user
// affordances without a request per component mount.
import { getToken } from './auth.ts'
import { getCurrentAccount } from './api.ts'
import type { mastodon } from './masto.ts'

const ID_KEY = 'eunha:me-id'
const ACCOUNT_KEY = 'eunha:me-account'

export interface MeAccount {
  id: string
  acct: string
  defaultVisibility: mastodon.v1.StatusVisibility
  // For the account card in the sidebar. Cached with the rest so the card
  // renders on first paint instead of appearing a request later.
  displayName: string
  avatar: string
  // The still avatar, shown unless GIFs auto-play or it is hovered. Absent
  // from an account cached before it was stored; until the next `loadMe`
  // fills it in, the card draws no image rather than an animating one.
  avatarStatic?: string
  // The display name's custom emoji.
  emojis?: mastodon.v1.CustomEmoji[]
  // The role's computed permissions, Mastodon's `UserRole::FLAGS` bitmask, for
  // deciding which moderation pages to offer. The server decides what each
  // admin request may do; this only keeps the rail from offering what it would
  // refuse. Absent from an account cached before it was stored, until the next
  // `loadMe` fills it in.
  permissions?: number
}

let cachedId: string | null = localStorage.getItem(ID_KEY)
let cachedAccount: MeAccount | null = readCachedAccount()

function readCachedAccount(): MeAccount | null {
  const raw = localStorage.getItem(ACCOUNT_KEY)
  if (!raw) return null

  try {
    const parsed = JSON.parse(raw) as Partial<MeAccount>
    return parsed.id && parsed.acct
      ? {
          id: parsed.id,
          acct: parsed.acct,
          defaultVisibility: isStatusVisibility(parsed.defaultVisibility)
            ? parsed.defaultVisibility
            : 'public',
          // A record cached before these were stored still has to parse; the
          // next `loadMe` fills them in.
          displayName: parsed.displayName ?? parsed.acct,
          avatar: parsed.avatar ?? '',
          avatarStatic: parsed.avatarStatic,
          emojis: Array.isArray(parsed.emojis) ? parsed.emojis : [],
          permissions: parsed.permissions,
        }
      : null
  } catch {
    return null
  }
}

function isStatusVisibility(
  value: unknown,
): value is mastodon.v1.StatusVisibility {
  return (
    value === 'public' ||
    value === 'unlisted' ||
    value === 'private' ||
    value === 'direct'
  )
}

export function getMeId(): string | null {
  return cachedId
}

export function getMeAccount(): MeAccount | null {
  return cachedAccount
}

export function getDefaultVisibility(): mastodon.v1.StatusVisibility {
  return cachedAccount?.defaultVisibility ?? 'public'
}

export async function loadMe(token: string): Promise<MeAccount | null> {
  try {
    const me = await getCurrentAccount(token)
    if (getToken() !== token) return null
    cachedId = me.id
    cachedAccount = {
      id: me.id,
      acct: me.acct,
      defaultVisibility: me.source.privacy ?? 'public',
      displayName: me.displayName || me.username,
      avatar: me.avatar,
      avatarStatic: me.avatarStatic,
      emojis: me.emojis,
      // masto.js does not model `role` on the credential account.
      permissions: Number(
        (me as unknown as { role?: { permissions?: string } }).role?.permissions ?? 0,
      ),
    }
    localStorage.setItem(ID_KEY, me.id)
    localStorage.setItem(ACCOUNT_KEY, JSON.stringify(cachedAccount))
    return cachedAccount
  } catch {
    // ignore — controls just won't render until known
    return cachedAccount
  }
}

export function clearMe(): void {
  cachedId = null
  cachedAccount = null
  localStorage.removeItem(ID_KEY)
  localStorage.removeItem(ACCOUNT_KEY)
}
