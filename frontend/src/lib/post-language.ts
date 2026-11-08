import { extractUrlsWithIndices, Extractor } from '@agulbra/uts58'

// Lande’s ISO 639-3 mapping follows Mastodon’s compose/util/language_detection.js.
const languageCodes: Record<string, string> = {
  afr: 'af', // Afrikaans
  ara: 'ar', // Arabic
  aze: 'az', // Azerbaijani
  bel: 'be', // Belarusian
  ben: 'bn', // Bengali
  bul: 'bg', // Bulgarian
  cat: 'ca', // Catalan
  ces: 'cs', // Czech
  ckb: 'ku', // Kurdish
  cmn: 'zh', // Mandarin
  dan: 'da', // Danish
  deu: 'de', // German
  ell: 'el', // Greek
  eng: 'en', // English
  est: 'et', // Estonian
  eus: 'eu', // Basque
  fin: 'fi', // Finnish
  fra: 'fr', // French
  hau: 'ha', // Hausa
  heb: 'he', // Hebrew
  hin: 'hi', // Hindi
  hrv: 'hr', // Croatian
  hun: 'hu', // Hungarian
  hye: 'hy', // Armenian
  ind: 'id', // Indonesian
  isl: 'is', // Icelandic
  ita: 'it', // Italian
  jpn: 'ja', // Japanese
  kat: 'ka', // Georgian
  kaz: 'kk', // Kazakh
  kor: 'ko', // Korean
  lit: 'lt', // Lithuanian
  mar: 'mr', // Marathi
  mkd: 'mk', // Macedonian
  nld: 'nl', // Dutch
  nob: 'no', // Norwegian
  pes: 'fa', // Persian
  pol: 'pl', // Polish
  por: 'pt', // Portuguese
  ron: 'ro', // Romanian
  run: 'rn', // Rundi
  rus: 'ru', // Russian
  slk: 'sk', // Slovak
  spa: 'es', // Spanish
  srp: 'sr', // Serbian
  swe: 'sv', // Swedish
  tgl: 'tl', // Tagalog
  tur: 'tr', // Turkish
  ukr: 'uk', // Ukrainian
  vie: 'vi', // Vietnamese
}

export type PostLanguage = [code: string, name: string, nativeName: string]

let languages: Promise<PostLanguage[]> | undefined

export function getPostLanguages() {
  languages ??= fetch('/api/eunha/v1/preferences/languages').then(async (response) => {
    if (!response.ok) throw new Error('Could not load post languages')
    return await response.json() as PostLanguage[]
  }).catch((error) => {
    languages = undefined
    throw error
  })
  return languages
}

const extractor = new Extractor()
const remoteMention = /(?<![/\p{L}\p{N}_])@([a-z0-9_]+(?:[a-z0-9_.-]*[a-z0-9_])?)@([^\s@/<>?#]+)/giu

function replaceUrls(text: string, replacement: string) {
  const urls = extractUrlsWithIndices(text, { extractUrlsWithoutProtocol: false })
  for (const { indices: [start, end] } of urls.reverse()) {
    text = text.slice(0, start) + replacement + text.slice(end)
  }
  return text
}

function replaceRemoteMentions(text: string, keepUsername: boolean) {
  return text.replace(remoteMention, (mention, username: string, host: string) => {
    const domain = host.replace(/[.,;:!?)\]}'"`]+$/, '')
    if (!extractor.isPlausibleHost(domain)) return mention
    return (keepUsername ? `@${username}` : '') + host.slice(domain.length)
  })
}

export async function guessPostLanguage(text: string) {
  const prose = replaceRemoteMentions(replaceUrls(text, ''), false).replace(/(?<![\p{L}\p{N}_])@[a-z0-9_]+/giu, '').trim()
  if (prose.length <= 20) return
  const { default: detect } = await import('lande')
  const best = detect(prose)[0]
  return best && best[1] > 0.8 ? languageCodes[best[0]] : undefined
}

const graphemes = new Intl.Segmenter(undefined, { granularity: 'grapheme' })

export function countGraphemes(text: string) {
  return Array.from(graphemes.segment(text)).length
}

export function countPostCharacters(text: string, warning = '', urlLength = 23) {
  const countable = text.trim() ? replaceRemoteMentions(replaceUrls(text, 'x'.repeat(urlLength)), true) : ''
  return countGraphemes(warning + countable)
}

export function postLanguageName(code: string) {
  try {
    return new Intl.DisplayNames(navigator.languages, { type: 'language' }).of(code) ?? code
  } catch {
    return code
  }
}
