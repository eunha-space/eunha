// Mastodon's messages at the release mastodon.toml tracks, as react-intl
// messages: its web client's catalogs, and its Rails locales under
// `mastodon.<key>`. Shared by import-mastodon-locales.mjs, check-locales.mjs
// and anything else that needs to know what an id means upstream.
//
// A Rails message becomes ICU: `%{name}` is `{name}`, a key whose children
// are plural forms (`one`, `other`, …) is a `{count, plural, …}` message, and
// an `_html` message is kept only when its markup is tags without attributes
// (`<strong>`, `<code>`), which react-intl renders as rich text. Messages that
// do not parse as ICU are left out.
import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { parse } from '@formatjs/icu-messageformat-parser'
import YAML from 'yaml'

const frontend = join(dirname(fileURLToPath(import.meta.url)), '..')
export const repo = process.env.MASTODON_REPO || join(homedir(), 'Git/mastodon')

const toml = readFileSync(join(frontend, '../mastodon.toml'), 'utf8')
export const version = toml.match(/^version\s*=\s*"([^"]+)"/m)?.[1]
if (!version) throw new Error('no [mastodon] version in mastodon.toml')
const tag = `v${version}`

const git = (...args) =>
  execFileSync('git', ['-C', repo, ...args], { encoding: 'utf8', maxBuffer: 64 << 20 })

/** Exit with directions when the checkout lacks the tracked tag. */
export function requireCheckout() {
  try {
    git('rev-parse', '-q', '--verify', `refs/tags/${tag}`)
  } catch {
    console.error(`No ${tag} in the Mastodon checkout at ${repo}.`)
    console.error('Clone https://github.com/mastodon/mastodon.git there, or set MASTODON_REPO.')
    process.exit(1)
  }
}

const PLURAL_FORMS = new Set(['zero', 'one', 'two', 'few', 'many', 'other'])

// Quote what ICU would read as syntax: an apostrophe before a brace, and a
// `#` inside a plural branch.
const escapeIcu = (text, inPlural) =>
  text.replace(/'(?=[{}#|'])/g, "''").replace(/#/g, inPlural ? "'#'" : '#')

const convert = (text, inPlural) =>
  escapeIcu(text, inPlural).replace(/%\{(\w+)\}/g, '{$1}')

const plainTags = (text) => [...text.matchAll(/<[^>]*>/g)].every((m) => /^<\/?[a-z]+>$/.test(m[0]))

function valid(message) {
  try {
    parse(message)
    return true
  } catch {
    return false
  }
}

function railsMessages(locale) {
  const files = git('ls-tree', '--name-only', tag, 'config/locales/')
    .split('\n')
    .filter((path) => path.endsWith(`/${locale}.yml`) || path.endsWith(`.${locale}.yml`))
  const out = {}
  const walk = (node, key) => {
    const entries = Object.entries(node)
    const forms = entries.filter(([k, v]) => PLURAL_FORMS.has(k) && typeof v === 'string')
    if (forms.length && forms.length === entries.length && forms.some(([k]) => k === 'other')) {
      if (forms.some(([, v]) => !plainTags(v))) return
      const branches = forms.map(([k, v]) => `${k} {${convert(v, true)}}`).join(' ')
      const message = `{count, plural, ${branches}}`
      if (valid(message)) out[`mastodon.${key}`] = message
      return
    }
    for (const [k, v] of entries) {
      const path = key ? `${key}.${k}` : k
      if (v && typeof v === 'object' && !Array.isArray(v)) walk(v, path)
      else if (typeof v === 'string' && v.trim() && plainTags(v)) {
        const message = convert(v, false)
        if (valid(message)) out[`mastodon.${path}`] = message
      }
    }
  }
  for (const file of files) {
    const doc = YAML.parse(git('show', `${tag}:${file}`))?.[locale]
    if (doc) walk(doc, '')
  }
  return out
}

function webMessages(locale) {
  try {
    return JSON.parse(git('show', `${tag}:app/javascript/mastodon/locales/${locale}.json`))
  } catch {
    return {}
  }
}

/** Every message Mastodon has for a locale, by the id eunha gives it. */
export function mastodonMessages(locale) {
  return { ...webMessages(locale), ...railsMessages(locale) }
}

/** The same text, however its apostrophes are quoted. */
export const sameMessage = (a, b) => a.replace(/''/g, "'") === b.replace(/''/g, "'")
