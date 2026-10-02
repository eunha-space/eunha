/** A policy's date as Mastodon's web app shows it: `Oct 02, 2026`. */
export function formatPolicyDate(value: string): string {
  // A bare date is a day, not an instant: read it in UTC so no time zone moves
  // it to the day before.
  const date = new Date(value.length === 10 ? `${value}T00:00:00Z` : value)
  if (Number.isNaN(date.getTime())) return value
  return date.toLocaleDateString(undefined, {
    year: 'numeric',
    month: 'short',
    day: '2-digit',
    timeZone: 'UTC',
  })
}
