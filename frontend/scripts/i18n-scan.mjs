// List the web client's text that does not go through react-intl: JSX text,
// text-bearing attributes, and sentences in string literals. With `--check`
// it exits non-zero when it finds any, which is how CI keeps new English out.
//
// A string that is not interface text (a CSS selector, a log line, a format)
// is skipped with a `// i18n-ignore` comment on its line or the line before;
// a file, with `// i18n-ignore-file` at its top.
//
//   node scripts/i18n-scan.mjs [--json] [--check] [files…]
import { readFileSync, readdirSync, statSync } from 'node:fs'
import { dirname, join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'
import ts from 'typescript'

const frontend = join(dirname(fileURLToPath(import.meta.url)), '..')
const src = join(frontend, 'src')
const args = process.argv.slice(2)
const json = args.includes('--json')
const check = args.includes('--check')
const only = args.filter((a) => !a.startsWith('--'))

// Generated shadcn components, and the module that defines translation.
const SKIP = [/\/components\/ui\//, /\/i18n\.tsx$/, /\.d\.ts$/]

// Attributes that never carry interface text.
const NOT_TEXT_ATTRS = new Set([
  'className', 'href', 'to', 'type', 'name', 'id', 'key', 'value', 'defaultValue', 'variant',
  'size', 'side', 'align', 'role', 'rel', 'target', 'autoComplete', 'inputMode', 'lang', 'dir',
  'method', 'action', 'accept', 'src', 'srcSet', 'style', 'htmlFor', 'form', 'pattern',
  'enterKeyHint', 'loading', 'decoding', 'mode', 'orientation', 'sizes', 'media', 'as',
  'encType', 'autoCapitalize', 'spellCheck', 'crossOrigin', 'referrerPolicy', 'sandbox',
  'allow', 'scope', 'path', 'to', 'state', 'storageKey', 'defaultTheme', 'theme', 'icon',
  'collapsible', 'wrap', 'step', 'min', 'max', 'minLength', 'maxLength', 'rows', 'cols',
  'tabIndex', 'kind', 'tone', 'status', 'format', 'locale', 'defaultLocale', 'list',
  'aria-hidden', 'aria-live', 'aria-current', 'aria-haspopup', 'aria-controls', 'aria-expanded',
  'aria-describedby', 'aria-labelledby', 'aria-owns', 'aria-autocomplete', 'aria-orientation',
  'aria-sort', 'aria-invalid', 'aria-pressed', 'aria-selected', 'aria-checked', 'aria-modal',
  'aria-busy', 'aria-disabled', 'aria-atomic', 'aria-relevant', 'download', 'datetime',
  'dateTime', 'xmlns', 'viewBox', 'fill', 'stroke', 'd', 'transform', 'preserveAspectRatio',
  'columns', 'section', 'tab', 'field', 'sort', 'filter', 'scheme', 'colorScheme', 'origin',
  'method', 'translate', 'slot', 'part', 'axis', 'event', 'feature', 'permission', 'unit',
])

// Calls whose string arguments are never shown.
const NOT_TEXT_CALLS = /^(console\.\w+|localStorage\.\w+|sessionStorage\.\w+|document\.\w+|.*\.querySelector(All)?|.*\.getAttribute|.*\.setAttribute|.*\.addEventListener|.*\.removeEventListener|.*\.startsWith|.*\.endsWith|.*\.includes|.*\.split|.*\.replace(All)?|.*\.indexOf|.*\.match|.*\.test|.*\.get|.*\.set|.*\.has|.*\.delete|.*\.append|fetch|call|route|.*\.route|cn|clsx|navigate|new URL|new URLSearchParams|matchMedia|.*\.matchMedia|getComputedStyle|.*\.getPropertyValue|t|.*\.formatMessage|defineMessages|defineMessage|import|require|.*\.toLocale\w*|Intl\.\w+|new Intl\.\w+|.*\.locator|.*\.getBy\w+|expect|.*\.json|.*\.text|.*\.postMessage|.*\.dispatchEvent|new CustomEvent|new Event|.*\.emit|.*\.on|.*\.off|.*\.open|window\.open|.*\.assign|.*\.push|.*\.pushState|.*\.replaceState|.*\.searchParams\.\w+|encodeURIComponent|decodeURIComponent|JSON\.\w+|.*\.padStart|.*\.padEnd|String|Number|Boolean|Symbol|.*\.join|.*\.at|.*\.slice|.*\.normalize|.*\.localeCompare|.*\.Fragment)$/

function walk(dir) {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name)
    if (statSync(path).isDirectory()) return walk(path)
    return /\.(ts|tsx)$/.test(name) ? [path] : []
  })
}

const hasWords = (text) => /[A-Za-z]{2,}/.test(text)
// A sentence or label, not an identifier, path, key or format.
const looksLikeText = (text) =>
  hasWords(text) &&
  !/^[a-z0-9_.:/#?&=@%+-]+$/i.test(text) && // keys, paths, ids
  !/^[\w-]+(\s+[\w-]+)*$/.test(text) || /^[A-Z][a-z]/.test(text) && hasWords(text)

function calleeName(expr) {
  if (ts.isIdentifier(expr)) return expr.text
  if (ts.isPropertyAccessExpression(expr)) return `${calleeName(expr.expression)}.${expr.name.text}`
  if (ts.isCallExpression(expr)) return `${calleeName(expr.expression)}()`
  if (ts.isNewExpression(expr)) return `new ${calleeName(expr.expression)}`
  return '?'
}

function ignored(sf, node, lines) {
  const line = sf.getLineAndCharacterOfPosition(node.getStart(sf)).line
  return /i18n-ignore/.test(lines[line] ?? '') || /i18n-ignore/.test(lines[line - 1] ?? '')
}

function insideTypeOrImport(node) {
  for (let n = node.parent; n; n = n.parent) {
    if (ts.isTypeNode(n) || ts.isImportDeclaration(n) || ts.isExportDeclaration(n)) return true
    if (ts.isLiteralTypeNode?.(n)) return true
  }
  return false
}

// Whether a node is evaluated when its module loads, rather than inside a
// function: such text cannot follow a change of language.
function atModuleLevel(node) {
  for (let n = node.parent; n; n = n.parent) {
    if (
      ts.isFunctionLike(n) ||
      ts.isClassLike(n) ||
      (ts.isPropertyAssignment(n) && false)
    ) {
      return false
    }
    if (ts.isGetAccessor(n)) return false
  }
  return true
}

function isBareDescriptor(node, sf) {
  const names = node.properties.map((p) => (p.name ? p.name.getText(sf) : ''))
  if (!names.includes('id') || !names.includes('defaultMessage')) return false
  let parent = node.parent
  // A member of `defineMessages({ … })`.
  if (ts.isPropertyAssignment(parent) && ts.isObjectLiteralExpression(parent.parent)) {
    const call = parent.parent.parent
    if (ts.isCallExpression(call) && calleeName(call.expression) === 'defineMessages') return false
  }
  if (ts.isAsExpression(parent) || ts.isSatisfiesExpression?.(parent)) parent = parent.parent
  return !(ts.isCallExpression(parent) && /^(t|.*\.formatMessage|defineMessage|formatMessage)$/.test(calleeName(parent.expression)))
}

export function scan(path) {
  const text = readFileSync(path, 'utf8')
  if (/i18n-ignore-file/.test(text.slice(0, 500))) return []
  const sf = ts.createSourceFile(path, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX)
  const lines = text.split('\n')
  const found = []
  const add = (node, kind, value, extra = {}) => {
    if (ignored(sf, node, lines)) return
    const { line, character } = sf.getLineAndCharacterOfPosition(node.getStart(sf))
    found.push({
      file: relative(frontend, path),
      line: line + 1,
      column: character + 1,
      start: node.getStart(sf),
      end: node.getEnd(),
      kind,
      text: value,
      moduleLevel: atModuleLevel(node),
      ...extra,
    })
  }

  const visit = (node) => {
    if (ts.isJsxText(node)) {
      const value = node.getText(sf).replace(/\s+/g, ' ').trim()
      if (hasWords(value)) add(node, 'jsx-text', value)
    } else if (ts.isJsxAttribute(node) && node.initializer) {
      const name = node.name.getText(sf)
      if (!NOT_TEXT_ATTRS.has(name) && !name.startsWith('data-')) {
        let init = node.initializer
        if (ts.isJsxExpression(init) && init.expression) init = init.expression
        if (ts.isStringLiteral(init) || ts.isNoSubstitutionTemplateLiteral(init)) {
          if (hasWords(init.text) && looksLikeText(init.text)) add(init, 'jsx-attr', init.text, { attr: name })
          return
        }
      }
    } else if (
      (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) &&
      !ts.isJsxAttribute(node.parent) &&
      !insideTypeOrImport(node)
    ) {
      const value = node.text
      const parent = node.parent
      // A sentence: words with a space, starting with a capital, or ending in
      // punctuation.
      const sentence = hasWords(value) && /\s/.test(value.trim()) && /^[A-Z]/.test(value.trim())
      const word = /^[A-Z][a-z]+[.…!?:]*$/.test(value)
      if (!(sentence || word)) return ts.forEachChild(node, visit)
      // Skip comparisons and switch cases: those read values, not text.
      if (ts.isBinaryExpression(parent) && /^(===|!==|==|!=|in|instanceof)$/.test(parent.operatorToken.getText(sf))) return
      if (ts.isCaseClause(parent)) return
      if (ts.isPropertyAssignment(parent) && parent.name === node) return
      // A message descriptor's own English.
      if (ts.isPropertyAssignment(parent) && /^(defaultMessage|description)$/.test(parent.name.getText(sf))) return
      if (ts.isElementAccessExpression(parent)) return
      if (ts.isCallExpression(parent) || ts.isNewExpression(parent)) {
        const callee = calleeName(parent.expression)
        if ((ts.isNewExpression(parent) ? `new ${callee}` : callee).match(NOT_TEXT_CALLS) || NOT_TEXT_CALLS.test(callee)) return
        add(node, 'string', value, { context: `call ${ts.isNewExpression(parent) ? 'new ' : ''}${callee}` })
        return
      }
      if (ts.isPropertyAssignment(parent)) {
        add(node, 'string', value, { context: `property ${parent.name.getText(sf)}` })
        return
      }
      add(node, word ? 'word' : 'string', value, { context: ts.SyntaxKind[parent.kind] })
      return
    } else if (ts.isTemplateExpression(node) && !insideTypeOrImport(node)) {
      const head = [node.head.text, ...node.templateSpans.map((s) => s.literal.text)].join('{}')
      const literal = head.replace(/\{\}/g, ' ').trim()
      // Class names, and an Authorization header's scheme.
      const classy = literal.split(/\s+/).every((w) => !w || /^[a-z0-9:_\-[\]/.!%#()&]+$/.test(w))
      // CSS values, and templates given to attributes that never carry text.
      const css = /\b(var\(--|calc\(|color-mix\(|translate\()/.test(literal)
      const attr = ts.isJsxExpression(node.parent) && ts.isJsxAttribute(node.parent.parent)
        ? node.parent.parent.name.getText(sf)
        : null
      if (classy || css || /^Bearer$/.test(literal) || (attr && (NOT_TEXT_ATTRS.has(attr) || attr.startsWith('data-')))) {
        return ts.forEachChild(node, visit)
      }
      if (hasWords(head) && /\s/.test(head) && /[A-Za-z]{3,}/.test(head.replace(/\{\}/g, ''))) {
        const parent = node.parent
        const callee = ts.isCallExpression(parent) ? calleeName(parent.expression) : ''
        if (!NOT_TEXT_CALLS.test(callee)) add(node, 'template', node.getText(sf))
      }
    } else if (ts.isObjectLiteralExpression(node) && isBareDescriptor(node, sf)) {
      // `i18n:extract` only finds descriptors given to `t`, `defineMessage(s)`
      // or `formatMessage`; one written bare never reaches the catalogs.
      add(node, 'bare-descriptor', node.getText(sf).slice(0, 80))
    } else if (ts.isCallExpression(node)) {
      const callee = calleeName(node.expression)
      // Text made when the module loads stays in the language it loaded in.
      if (callee === 't' && atModuleLevel(node)) add(node, 'module-t', node.getText(sf).slice(0, 80))
      if (/^(t|.*\.formatMessage|defineMessages|defineMessage)$/.test(callee)) {
        // A descriptor chosen inside the call (`t(ok ? a : b)`) is not found
        // by the extractor either.
        const first = node.arguments[0]
        if (first && !ts.isObjectLiteralExpression(first)) {
          const find = (n) => {
            if (ts.isObjectLiteralExpression(n) && isBareDescriptor(n, sf)) add(n, 'bare-descriptor', n.getText(sf).slice(0, 80))
            else ts.forEachChild(n, find)
          }
          find(first)
        }
        return
      }
      if (/^console\./.test(callee)) return
    } else if (ts.isJsxElement(node) || ts.isJsxSelfClosingElement(node)) {
      const tag = (ts.isJsxElement(node) ? node.openingElement : node).tagName.getText(sf)
      if (tag === 'FormattedMessage' || tag === 'code' || tag === 'kbd' || tag === 'pre') return
    }
    ts.forEachChild(node, visit)
  }
  visit(sf)
  return found
}

export const scannedFiles = () =>
  walk(src).filter((f) => !SKIP.some((re) => re.test(f)))

if (process.argv[1] !== fileURLToPath(import.meta.url)) {
  // Imported, by the codemod: nothing to print.
} else {
const files = (only.length ? only.map((f) => join(frontend, f)) : walk(src)).filter(
  (f) => !SKIP.some((re) => re.test(f)),
)
const results = files.flatMap(scan)

if (json) {
  console.log(JSON.stringify(results, null, 2))
} else {
  for (const r of results) {
    console.log(`${r.file}:${r.line}:${r.column} ${r.kind}${r.attr ? `[${r.attr}]` : ''}${r.context ? ` (${r.context})` : ''}${r.moduleLevel ? ' MODULE' : ''}  ${r.text}`)
  }
  console.error(`${results.length} untranslated in ${new Set(results.map((r) => r.file)).size} files`)
}
if (check && results.length) process.exit(1)
}
