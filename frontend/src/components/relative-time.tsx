import { useEffect, useMemo, useState } from 'react'

const SECOND = 1000
const MINUTE = 60 * SECOND
const HOUR = 60 * MINUTE
const DAY = 24 * HOUR
const WEEK = 7 * DAY

const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' })

function nextUpdateDelay(diff: number) {
  const abs = Math.abs(diff)

  if (abs < MINUTE) return SECOND
  if (abs < HOUR) return MINUTE
  if (abs < DAY) return 5 * MINUTE
  return HOUR
}

export function formatRelativeTime(date: Date, now = new Date()) {
  const diff = date.getTime() - now.getTime()
  const abs = Math.abs(diff)

  if (Number.isNaN(diff)) return ''
  if (abs < 5 * SECOND) return 'now'
  if (abs < MINUTE) return rtf.format(Math.round(diff / SECOND), 'second')
  if (abs < HOUR) return rtf.format(Math.round(diff / MINUTE), 'minute')
  if (abs < DAY) return rtf.format(Math.round(diff / HOUR), 'hour')
  if (abs < WEEK) return rtf.format(Math.round(diff / DAY), 'day')

  // Past a week a count of days stops meaning anything ("1,463 days ago"),
  // so show the date, as Mastodon's web UI does: "Mar 5", or "Mar 5, 2022"
  // outside the current year.
  return date.toLocaleDateString(undefined, {
    month: 'short',
    day: 'numeric',
    year: date.getFullYear() === now.getFullYear() ? undefined : 'numeric',
  })
}

export function RelativeTime({ value }: { value: string | Date }) {
  const date = useMemo(() => new Date(value), [value])
  const [now, setNow] = useState(() => new Date())

  useEffect(() => {
    if (Number.isNaN(date.getTime())) return

    const timeout = window.setTimeout(
      () => setNow(new Date()),
      nextUpdateDelay(date.getTime() - Date.now()),
    )

    return () => window.clearTimeout(timeout)
  }, [date, now])

  const label = formatRelativeTime(date, now)

  if (!label) return null

  return (
    <time dateTime={date.toISOString()} title={date.toLocaleString()}>
      {label}
    </time>
  )
}
