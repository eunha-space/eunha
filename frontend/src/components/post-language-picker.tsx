import { useEffect, useState } from 'react'
import { Languages } from 'lucide-react'

import { getActiveAccountId } from '../auth.ts'
import { getPostLanguages, postLanguageName, type PostLanguage } from '@/lib/post-language.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover.tsx'

function readFrequency(key: string): Record<string, number> {
  try {
    return JSON.parse(localStorage.getItem(key) ?? '{}') ?? {}
  } catch {
    return {}
  }
}

export function PostLanguagePicker({
  value,
  onChange,
  label = 'Post language',
  disabled,
  allowDefault,
}: {
  value: string
  onChange: (code: string) => void
  label?: string
  disabled?: boolean
  allowDefault?: boolean
}) {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [options, setOptions] = useState<PostLanguage[]>()
  const [error, setError] = useState(false)
  const frequencyKey = `eunha:post-language-frequency:${getActiveAccountId()}`

  useEffect(() => {
    if (!open) return
    let active = true
    setError(false)
    const frequent = readFrequency(frequencyKey)
    getPostLanguages().then((languages) => {
      if (active) {
        setOptions([...languages].sort((a, b) =>
          Number(b[0] === value) - Number(a[0] === value)
          || (frequent[b[0]] ?? 0) - (frequent[a[0]] ?? 0)
          || a[1].localeCompare(b[1]),
        ))
      }
    }).catch(() => { if (active) setError(true) })
    return () => { active = false }
  }, [open, frequencyKey, value])

  const choose = (code: string) => {
    if (!allowDefault && code) {
      try {
        const frequent = readFrequency(frequencyKey)
        frequent[code] = (frequent[code] ?? 0) + 1
        localStorage.setItem(frequencyKey, JSON.stringify(frequent))
      } catch {
        // The selection still works when browser storage is unavailable.
      }
    }
    onChange(code)
    setOpen(false)
  }
  const matches = options?.filter((language) =>
    language.some((part) => part.toLocaleLowerCase().includes(query.trim().toLocaleLowerCase())),
  )

  return (
    <Popover open={open} onOpenChange={(next) => { setOpen(next); setQuery('') }}>
      <PopoverTrigger
        render={<Button type="button" size="sm" variant="outline" disabled={disabled} aria-label={label} />}
      >
        <Languages /> {value ? postLanguageName(value) : allowDefault ? 'Use interface language' : label}
      </PopoverTrigger>
      <PopoverContent aria-label={label} align="start">
        <Input aria-label="Search languages" placeholder="Search languages…" value={query}
          onChange={(event) => setQuery(event.target.value)} />
        {allowDefault && !query && (
          <Button type="button" variant="ghost" aria-pressed={!value} onClick={() => choose('')}>
            Use interface language
          </Button>
        )}
        {error && <p role="alert">Could not load languages. Reopen the picker to retry.</p>}
        {!matches && !error && <p className="text-muted-foreground">Loading…</p>}
        {matches?.length === 0 && <p className="text-muted-foreground">No languages found.</p>}
        <div className="max-h-64 overflow-y-auto">
          {matches?.map(([code, name, nativeName]) => (
            <Button key={code} type="button" variant="ghost" aria-pressed={value === code}
              className="h-auto w-full justify-start whitespace-normal text-left" onClick={() => choose(code)}>
              {name === nativeName ? name : `${name} · ${nativeName}`}
            </Button>
          ))}
        </div>
      </PopoverContent>
    </Popover>
  )
}
