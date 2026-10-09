// How the signed-in account wants posts shown: the three appearance settings
// Mastodon's web client reads from its initial state (`display_media`,
// `expand_spoilers`, `auto_play_gif`). Mastodon renders them into the page, so
// they are there on first paint; here they come from the preferences API and
// are cached per account, so a reload paints with them too.
import { useSyncExternalStore } from 'react'

import { getActiveAccountId, getToken } from './auth.ts'
import { getPreferences, type Preferences } from './security-api.ts'

export interface ReadingPreferences {
  /** `web.display_media`: hide sensitive media, show all, or hide all. */
  displayMedia: Preferences['display_media']
  /** `web.expand_content_warnings`: posts with a content warning start open. */
  expandSpoilers: boolean
  /** `web.auto_play`: animated GIFs, avatars and headers play without a hover. */
  autoPlayGif: boolean
}

// `UserSettings`' defaults. A signed-out visitor gets them too: Mastodon's
// initial state then reads `Setting.display_media` and `Setting.auto_play_gif`,
// which no setting defines, so its client falls back to the same behaviour.
export const DEFAULT_READING_PREFERENCES: ReadingPreferences = {
  displayMedia: 'default',
  expandSpoilers: false,
  autoPlayGif: false,
}

const cacheKey = (accountId: string) => `eunha:reading-preferences:${accountId}`

function readCached(accountId: string | null): ReadingPreferences {
  if (!accountId || !getToken()) return DEFAULT_READING_PREFERENCES
  try {
    const parsed = JSON.parse(localStorage.getItem(cacheKey(accountId)) ?? 'null') as
      | Partial<ReadingPreferences>
      | null
    if (!parsed) return DEFAULT_READING_PREFERENCES
    return {
      displayMedia:
        parsed.displayMedia === 'show_all' || parsed.displayMedia === 'hide_all'
          ? parsed.displayMedia
          : 'default',
      expandSpoilers: parsed.expandSpoilers === true,
      autoPlayGif: parsed.autoPlayGif === true,
    }
  } catch {
    return DEFAULT_READING_PREFERENCES
  }
}

let current = readCached(getActiveAccountId())
const listeners = new Set<() => void>()

function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

/** The active account's reading preferences, outside React. */
export function getReadingPreferences(): ReadingPreferences {
  return current
}

/** The active account's reading preferences; re-renders when they change. */
export function useReadingPreferences(): ReadingPreferences {
  return useSyncExternalStore(subscribe, getReadingPreferences)
}

/**
 * Takes what the preferences API returned for the active account, from the
 * load at start-up or from a save on the settings page.
 */
export function applyPreferences(prefs: Preferences) {
  const next: ReadingPreferences = {
    displayMedia: prefs.display_media,
    expandSpoilers: prefs.expand_content_warnings,
    autoPlayGif: prefs.auto_play,
  }
  const accountId = getActiveAccountId()
  if (accountId) {
    try {
      localStorage.setItem(cacheKey(accountId), JSON.stringify(next))
    } catch {
      // the cache only saves a request's wait on the next load
    }
  }
  if (
    next.displayMedia === current.displayMedia &&
    next.expandSpoilers === current.expandSpoilers &&
    next.autoPlayGif === current.autoPlayGif
  ) {
    return
  }
  current = next
  for (const listener of listeners) listener()
}

/** Fetches the active account's preferences, once per page load. */
export async function loadReadingPreferences(token: string): Promise<void> {
  try {
    const prefs = await getPreferences(token)
    // Another account may have become active while this was in flight.
    if (getToken() !== token) return
    applyPreferences(prefs)
  } catch {
    // keep the cached or default preferences
  }
}

/**
 * Whether media starts shown, as Mastodon's `defaultMediaVisibility` decides:
 * a media filter always hides it, `show_all` otherwise shows it, `hide_all`
 * hides it, and by default only media marked sensitive is hidden.
 */
export function mediaShownByDefault(
  prefs: ReadingPreferences,
  sensitive: boolean,
  filtered = false,
): boolean {
  if (filtered) return false
  return (
    prefs.displayMedia === 'show_all' || (prefs.displayMedia !== 'hide_all' && !sensitive)
  )
}
