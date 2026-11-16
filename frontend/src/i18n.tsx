// The web client's interface language.
//
// Messages are react-intl's, as in Mastodon's web client, and where a message
// means what one of Mastodon's does it carries Mastodon's id, so Mastodon's
// translations of it can be used (`pnpm run i18n:import-mastodon`). Messages
// of eunha's own carry ids under `eunha.`. English is the default message in
// the code; each other locale is Mastodon's catalog for the ids eunha reuses,
// under eunha's own, which win.
//
// Which locale: the signed-in account's interface language (`users.locale`,
// which also sets the language of its mail), then the one chosen in this
// browser, then the browser's own languages, then English. The account's
// is cached so a reload paints in it before the preferences API answers.
import {
  createContext,
  Fragment,
  useCallback,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from 'react'
import {
  createIntl,
  createIntlCache,
  IntlProvider,
  type IntlShape,
  type MessageDescriptor,
} from 'react-intl'

import { getActiveAccountId, getToken } from './auth.ts'
import { getPreferences, updatePreferences } from './security-api.ts'

export const LOCALES = [
  { code: 'en', name: 'English' },
  { code: 'ko', name: '한국어' },
] as const

export type Locale = (typeof LOCALES)[number]['code']

type Messages = Record<string, string>

const isLocale = (value: unknown): value is Locale =>
  LOCALES.some((locale) => locale.code === value)

const BROWSER_KEY = 'eunha:locale'
const accountKey = (accountId: string) => `eunha:locale:${accountId}`

function read(key: string): string | null {
  try {
    return localStorage.getItem(key)
  } catch {
    return null
  }
}

function write(key: string, value: string) {
  try {
    localStorage.setItem(key, value)
  } catch {
    // Private windows and blocked storage: the choice lasts the page.
  }
}

// One of the browser's languages eunha has, by its language alone, the way
// Mastodon matches `Accept-Language` against its available locales.
function fromBrowser(): Locale | null {
  for (const tag of navigator.languages ?? [navigator.language]) {
    const language = tag.split(/[_-]/)[0]?.toLowerCase()
    if (isLocale(language)) return language
  }
  return null
}

export function initialLocale(): Locale {
  const accountId = getToken() ? getActiveAccountId() : null
  const cached = accountId ? read(accountKey(accountId)) : null
  if (isLocale(cached)) return cached
  const chosen = read(BROWSER_KEY)
  if (isLocale(chosen)) return chosen
  return fromBrowser() ?? 'en'
}

const catalogs = import.meta.glob<{ default: Messages }>('./locales/*.json')
const mastodonCatalogs = import.meta.glob<{ default: Messages }>('./locales/mastodon/*.json')

export async function loadMessages(locale: Locale): Promise<Messages> {
  if (locale === 'en') return {}
  const [mastodon, own] = await Promise.all([
    mastodonCatalogs[`./locales/mastodon/${locale}.json`]?.(),
    catalogs[`./locales/${locale}.json`]?.(),
  ])
  return { ...mastodon?.default, ...own?.default }
}

function applyToDocument(locale: Locale) {
  document.documentElement.lang = locale
}

// The same messages outside React: in helpers, toasts, and the labels of
// lists defined beside a component. Changing the language remounts the app
// (see `LocaleProvider`), so text from `t` follows it as `useIntl`'s does.
// Text built when a module loads would not, so modules keep descriptors and
// call `t` when they render.
const cache = createIntlCache()
let current: IntlShape = createIntl({ locale: 'en', defaultLocale: 'en', messages: {} }, cache)

type Values = Record<string, string | number | boolean | null | undefined | Date>

export function t(descriptor: MessageDescriptor, values?: Values): string {
  return current.formatMessage(descriptor, values)
}

/** The interface locale, for `Intl` formatting outside React. */
export const currentLocale = (): Locale => current.locale as Locale

function setCurrent(locale: Locale, messages: Messages) {
  current = createIntl(
    { locale, defaultLocale: 'en', messages, onError: ignoreMissingTranslations },
    cache,
  )
}

type LocaleState = {
  locale: Locale
  /** Switch the interface, and keep the choice: in this browser, and on the
   *  signed-in account. */
  setLocale: (locale: Locale) => Promise<void>
}

const LocaleContext = createContext<LocaleState>({
  locale: 'en',
  setLocale: async () => {},
})

export const useLocale = () => useContext(LocaleContext)

// A message missing from a catalog falls back to its English default, as in
// Mastodon; react-intl would otherwise report each one on the console.
const ignoreMissingTranslations = (error: { code?: string }) => {
  if (error.code === 'MISSING_TRANSLATION') return
  console.error(error)
}

export function LocaleProvider({
  initial,
  initialMessages,
  children,
}: {
  initial: Locale
  initialMessages: Messages
  children: ReactNode
}) {
  const [state, setState] = useState({ locale: initial, messages: initialMessages })

  const show = useCallback(async (locale: Locale) => {
    const messages = await loadMessages(locale)
    applyToDocument(locale)
    setCurrent(locale, messages)
    setState({ locale, messages })
  }, [])

  // The account's own setting, once the preferences API answers. None set
  // leaves the interface as it is.
  useEffect(() => {
    const token = getToken()
    const accountId = getActiveAccountId()
    if (!token || !accountId) return
    let live = true
    getPreferences(token)
      .then((preferences) => {
        if (!live || !isLocale(preferences.locale)) return
        write(accountKey(accountId), preferences.locale)
        if (preferences.locale !== state.locale) void show(preferences.locale)
      })
      .catch(() => {})
    return () => {
      live = false
    }
    // Once per page: switching accounts reloads it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const setLocale = useCallback(
    async (locale: Locale) => {
      write(BROWSER_KEY, locale)
      const token = getToken()
      const accountId = getActiveAccountId()
      if (token && accountId) {
        write(accountKey(accountId), locale)
        await show(locale)
        await updatePreferences(token, { locale })
      } else {
        await show(locale)
      }
    },
    [show],
  )

  return (
    <LocaleContext.Provider value={{ locale: state.locale, setLocale }}>
      <IntlProvider
        locale={state.locale}
        defaultLocale="en"
        messages={state.messages}
        onError={ignoreMissingTranslations}
      >
        {/* Remounted on a change of language, so text from `t` follows. */}
        <Fragment key={state.locale}>{children}</Fragment>
      </IntlProvider>
    </LocaleContext.Provider>
  )
}

/** Before the first render, so the page paints in its language. */
export async function bootLocale(): Promise<{ locale: Locale; messages: Messages }> {
  const locale = initialLocale()
  applyToDocument(locale)
  const messages: Messages = await loadMessages(locale).catch(() => ({}))
  setCurrent(locale, messages)
  return { locale, messages }
}
