// Fail when the message catalogs have fallen behind the code:
//
//  -  src/locales/en.json is not what `pnpm run i18n:extract` makes of it;
//  -  a locale's own catalog lacks one of eunha's messages, or keeps one the
//     code no longer has;
//  -  a message reusing a Mastodon id is in neither catalog of a locale, which
//     is what forgetting `pnpm run i18n:import-mastodon` looks like.
//
//   pnpm run i18n:check
import { execFileSync } from 'node:child_process'
import { mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const frontend = join(dirname(fileURLToPath(import.meta.url)), '..')
const locales = join(frontend, 'src/locales')
const read = (path) => JSON.parse(readFileSync(path, 'utf8'))
const problems = []

const scratch = mkdtempSync(join(tmpdir(), 'eunha-i18n-'))
try {
  execFileSync('pnpm', ['run', '--silent', 'i18n:extract', '--out-file', join(scratch, 'en.json')], {
    cwd: frontend,
    stdio: 'inherit',
  })
  const extracted = readFileSync(join(scratch, 'en.json'), 'utf8')
  if (extracted !== readFileSync(join(locales, 'en.json'), 'utf8')) {
    problems.push('src/locales/en.json is out of date: run `pnpm run i18n:extract`')
  }
} finally {
  rmSync(scratch, { recursive: true, force: true })
}

const english = read(join(locales, 'en.json'))
const ids = Object.keys(english)
for (const file of readdirSync(locales).filter((f) => f.endsWith('.json') && f !== 'en.json')) {
  const locale = file.replace(/\.json$/, '')
  const own = read(join(locales, file))
  const mastodon = read(join(locales, 'mastodon', file))
  for (const id of ids) {
    if (id.startsWith('eunha.') && !(id in own)) problems.push(`${locale}: no translation of ${id}`)
    if (!id.startsWith('eunha.') && !(id in own) && !(id in mastodon)) {
      problems.push(`${locale}: ${id} is Mastodon's but not imported: run \`pnpm run i18n:import-mastodon\``)
    }
  }
  for (const id of Object.keys(own)) {
    if (!(id in english)) problems.push(`${locale}: ${id} is no longer in the code`)
  }
}

if (problems.length) {
  for (const problem of problems) console.error(problem)
  process.exit(1)
}
console.log(`${ids.length} messages, every locale up to date`)
