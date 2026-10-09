// A host name's `xn--` labels in the letters they stand for, as Mastodon's
// card shows a provider it has no name for (`decodeIDNA`). Browsers hand out
// the ASCII form; this is RFC 3492's decoder.
const BASE = 36
const T_MIN = 1
const T_MAX = 26
const SKEW = 38
const DAMP = 700
const INITIAL_BIAS = 72
const INITIAL_N = 128

function adapt(delta: number, points: number, first: boolean): number {
  delta = first ? Math.floor(delta / DAMP) : delta >> 1
  delta += Math.floor(delta / points)
  let k = 0
  while (delta > ((BASE - T_MIN) * T_MAX) >> 1) {
    delta = Math.floor(delta / (BASE - T_MIN))
    k += BASE
  }
  return k + Math.floor(((BASE - T_MIN + 1) * delta) / (delta + SKEW))
}

function digit(code: number): number {
  if (code >= 48 && code < 58) return code - 22
  if (code >= 65 && code < 91) return code - 65
  if (code >= 97 && code < 123) return code - 97
  return BASE
}

function decodeLabel(input: string): string {
  const output: number[] = []
  const basic = input.lastIndexOf('-')
  for (let j = 0; j < Math.max(basic, 0); j++) output.push(input.charCodeAt(j))
  let n = INITIAL_N
  let bias = INITIAL_BIAS
  let i = 0
  for (let index = basic > 0 ? basic + 1 : 0; index < input.length; ) {
    const oldi = i
    for (let w = 1, k = BASE; ; k += BASE) {
      if (index >= input.length) throw new RangeError('bad punycode')
      const d = digit(input.charCodeAt(index++))
      if (d >= BASE) throw new RangeError('bad punycode')
      i += d * w
      const t = k <= bias ? T_MIN : k >= bias + T_MAX ? T_MAX : k - bias
      if (d < t) break
      w *= BASE - t
    }
    const length = output.length + 1
    bias = adapt(i - oldi, length, oldi === 0)
    n += Math.floor(i / length)
    i %= length
    output.splice(i++, 0, n)
  }
  return String.fromCodePoint(...output)
}

export function decodeIdna(domain: string): string {
  return domain
    .split('.')
    .map((part) => {
      if (!part.toLowerCase().startsWith('xn--')) return part
      try {
        return decodeLabel(part.slice(4))
      } catch {
        return part
      }
    })
    .join('.')
}
