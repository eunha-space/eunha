// Copy Mastodon's translations of the messages eunha reuses into
// src/locales/mastodon/<locale>.json, from the release mastodon.toml tracks.
//
// A message reuses Mastodon's when its id is not under `eunha.`: a web client
// id as it is, or `mastodon.<key>` for a key of Mastodon's Rails locales (see
// mastodon-messages.mjs). The ids come from src/locales/en.json, so run
// `pnpm run i18n:extract` first. The Mastodon checkout is $MASTODON_REPO, or
// ~/Git/mastodon, as for `mise run mastodon:changes`.
//
//   pnpm run i18n:import-mastodon
import { readFileSync, readdirSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { mastodonMessages, requireCheckout, version } from './mastodon-messages.mjs'

const frontend = join(dirname(fileURLToPath(import.meta.url)), '..')
const locales = join(frontend, 'src/locales')

requireCheckout()

const english = JSON.parse(readFileSync(join(locales, 'en.json'), 'utf8'))
const reused = Object.keys(english).filter((id) => !id.startsWith('eunha.'))
const upstreamEnglish = mastodonMessages('en')
const unknown = reused.filter((id) => !(id in upstreamEnglish))
if (unknown.length) {
  console.error(`Not message ids in Mastodon ${version}: ${unknown.join(', ')}`)
  process.exit(1)
}

// Every locale eunha has a catalog of its own for, besides English.
const targets = readdirSync(locales)
  .filter((file) => file.endsWith('.json') && file !== 'en.json')
  .map((file) => file.replace(/\.json$/, ''))

for (const locale of targets) {
  const upstream = mastodonMessages(locale)
  const catalog = Object.fromEntries(
    reused.filter((id) => id in upstream).sort().map((id) => [id, upstream[id]]),
  )
  writeFileSync(join(locales, 'mastodon', `${locale}.json`), `${JSON.stringify(catalog, null, 2)}\n`)
  const missing = reused.filter((id) => !(id in upstream))
  console.log(
    `${locale}: ${Object.keys(catalog).length} from Mastodon ${version}` +
      (missing.length ? `; untranslated there: ${missing.length}` : ''),
  )
}
