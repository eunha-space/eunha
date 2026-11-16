// Fail when the message catalogs have fallen behind the code:
//
//  -  src/locales/en.json is not what `pnpm run i18n:extract` makes of it;
//  -  a locale's own catalog lacks one of eunha's messages, or keeps one the
//     code no longer has;
//  -  a message reusing a Mastodon id is in neither catalog of a locale, which
//     is what forgetting `pnpm run i18n:import-mastodon` looks like;
//  -  a translation does not take the placeholders and tags its English does;
//  -  the code has text that does not go through react-intl (i18n-scan.mjs).
//
//   pnpm run i18n:check
import { execFileSync } from 'node:child_process'
import { mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { parse, TYPE } from '@formatjs/icu-messageformat-parser'

import { scan, scannedFiles } from './i18n-scan.mjs'

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

for (const hit of scannedFiles().flatMap(scan)) {
  problems.push(`${hit.file}:${hit.line}: not translatable: ${hit.text}`)
}

// The placeholders and tags a message takes, which every translation of it
// must take too (a plural's `#` aside).
function argumentsOf(message) {
  const names = new Set()
  const visit = (elements) => {
    for (const el of elements) {
      if (el.type === TYPE.argument || el.type === TYPE.number || el.type === TYPE.date || el.type === TYPE.time) names.add(el.value)
      if (el.type === TYPE.select || el.type === TYPE.plural) {
        names.add(el.value)
        for (const option of Object.values(el.options)) visit(option.value)
      }
      if (el.type === TYPE.tag) {
        names.add(`<${el.value}>`)
        visit(el.children)
      }
    }
  }
  visit(parse(message))
  return [...names].sort().join(' ')
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
      problems.push(`${locale}: ${id} is in neither catalog: run \`pnpm run i18n:import-mastodon\`, or translate it in ${file}`)
    }
  }
  for (const id of Object.keys(own)) {
    if (!(id in english)) problems.push(`${locale}: ${id} is no longer in the code`)
  }
  for (const [id, message] of Object.entries({ ...mastodon, ...own })) {
    if (!(id in english)) continue
    let theirs
    try {
      theirs = argumentsOf(message)
    } catch (error) {
      problems.push(`${locale}: ${id} does not parse: ${error.message}`)
      continue
    }
    const ours = argumentsOf(english[id])
    // Mastodon's own translations may leave out a placeholder their English
    // has (a plural's count, say); eunha's may not.
    const extra = theirs.split(' ').filter((name) => name && !ours.split(' ').includes(name))
    if (extra.length || (id in own && theirs !== ours)) {
      problems.push(`${locale}: ${id} takes {${theirs}}, its English {${ours}}`)
    }
  }
}

if (problems.length) {
  for (const problem of problems) console.error(problem)
  process.exit(1)
}
console.log(`${ids.length} messages, every locale up to date`)
