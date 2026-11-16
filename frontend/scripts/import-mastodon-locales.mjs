// Copy Mastodon's translations of the messages eunha reuses into
// src/locales/mastodon/<locale>.json, from the release mastodon.toml tracks.
//
// A message reuses Mastodon's when its id is not under `eunha.`; the ids come
// from src/locales/en.json, so run `pnpm run i18n:extract` first. The
// Mastodon checkout is $MASTODON_REPO, or ~/Git/mastodon, as for
// `mise run mastodon:changes`.
//
//   pnpm run i18n:import-mastodon
import { execFileSync } from 'node:child_process'
import { readFileSync, readdirSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const frontend = join(dirname(fileURLToPath(import.meta.url)), '..')
const locales = join(frontend, 'src/locales')
const repo = process.env.MASTODON_REPO || join(homedir(), 'Git/mastodon')

const toml = readFileSync(join(frontend, '../mastodon.toml'), 'utf8')
const version = toml.match(/^version\s*=\s*"([^"]+)"/m)?.[1]
if (!version) throw new Error('no [mastodon] version in mastodon.toml')
const tag = `v${version}`

const git = (...args) => execFileSync('git', ['-C', repo, ...args], { encoding: 'utf8' })
try {
  git('rev-parse', '-q', '--verify', `refs/tags/${tag}`)
} catch {
  console.error(`No ${tag} in the Mastodon checkout at ${repo}.`)
  console.error('Clone https://github.com/mastodon/mastodon.git there, or set MASTODON_REPO.')
  process.exit(1)
}

const english = JSON.parse(readFileSync(join(locales, 'en.json'), 'utf8'))
const reused = Object.keys(english).filter((id) => !id.startsWith('eunha.'))
const upstreamEnglish = JSON.parse(git('show', `${tag}:app/javascript/mastodon/locales/en.json`))
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
  const upstream = JSON.parse(
    git('show', `${tag}:app/javascript/mastodon/locales/${locale}.json`),
  )
  const catalog = Object.fromEntries(
    reused.filter((id) => id in upstream).sort().map((id) => [id, upstream[id]]),
  )
  writeFileSync(join(locales, 'mastodon', `${locale}.json`), `${JSON.stringify(catalog, null, 2)}\n`)
  const missing = reused.filter((id) => !(id in upstream))
  console.log(
    `${locale}: ${Object.keys(catalog).length} from Mastodon ${version}` +
      (missing.length ? `; untranslated there: ${missing.join(', ')}` : ''),
  )
}
