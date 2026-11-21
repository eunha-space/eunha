// masto.js hands entities back in camelCase; an entity fetched without it,
// from eunha's own routes or a page's embedded data, is made to match.
export function camelize(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(camelize)
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value).map(([key, v]) => [
        key.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase()),
        camelize(v),
      ]),
    )
  }
  return value
}
