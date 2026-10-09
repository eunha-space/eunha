import { useMemo, useState, type ReactElement } from 'react'
import { Smile } from 'lucide-react'

import type { mastodon } from '../masto.ts'
import { searchCustomEmojis, useCustomEmojis } from '@/hooks/use-custom-emojis.ts'
import { useReadingPreferences } from '../reading-preferences.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover.tsx'

function EmojiButton({ emoji, onPick }: { emoji: mastodon.v1.CustomEmoji; onPick: () => void }) {
  const { autoPlayGif } = useReadingPreferences()
  const [hovering, setHovering] = useState(false)
  return (
    <button
      type="button"
      title={`:${emoji.shortcode}:`}
      aria-label={`:${emoji.shortcode}:`}
      onClick={onPick}
      onMouseEnter={() => setHovering(true)}
      onMouseLeave={() => setHovering(false)}
      className="hover:bg-accent focus-visible:ring-ring flex size-8 items-center justify-center rounded-md outline-none focus-visible:ring-2"
    >
      <img
        src={autoPlayGif || hovering ? emoji.url : emoji.staticUrl}
        alt=""
        loading="lazy"
        className="size-6 object-contain"
      />
    </button>
  )
}

/**
 * The server's custom emoji by category, with a search, as the custom part of
 * Mastodon's emoji picker. `onPick` is handed the shortcode.
 */
export function EmojiPicker({
  onPick,
  trigger,
  label = 'Insert custom emoji',
}: {
  onPick: (shortcode: string) => void
  /** The button that opens it; an icon button by default. */
  trigger?: ReactElement
  label?: string
}) {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const emojis = useCustomEmojis(open)
  const groups = useMemo(() => {
    const shown = query.trim() ? searchCustomEmojis(emojis, query.trim()) : emojis
    if (query.trim()) return [['', shown] as const]
    const byCategory = new Map<string, mastodon.v1.CustomEmoji[]>()
    for (const emoji of shown) {
      const category = emoji.category ?? ''
      byCategory.set(category, [...(byCategory.get(category) ?? []), emoji])
    }
    // Uncategorized first, as Mastodon's picker lists them under "Custom".
    return [...byCategory.entries()].sort(([a], [b]) =>
      a === '' ? -1 : b === '' ? 1 : a.localeCompare(b),
    )
  }, [emojis, query])

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next)
        if (!next) setQuery('')
      }}
    >
      <PopoverTrigger
        render={
          trigger ?? (
            <Button type="button" variant="ghost" size="icon" aria-label={label}>
              <Smile />
            </Button>
          )
        }
      />
      <PopoverContent align="start" className="w-80 gap-2">
        <Input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search emoji"
          aria-label="Search emoji"
          className="h-8"
        />
        <div className="max-h-64 space-y-2 overflow-y-auto">
          {groups.map(([category, list]) => (
            <section key={category} aria-label={category || 'Custom'}>
              <h3 className="text-muted-foreground mb-1 text-xs font-medium">
                {category || (query.trim() ? 'Results' : 'Custom')}
              </h3>
              <div className="flex flex-wrap gap-0.5">
                {list.map((emoji) => (
                  <EmojiButton
                    key={emoji.shortcode}
                    emoji={emoji}
                    onPick={() => {
                      onPick(emoji.shortcode)
                      setOpen(false)
                      setQuery('')
                    }}
                  />
                ))}
              </div>
            </section>
          ))}
          {groups.every(([, list]) => list.length === 0) && (
            <p className="text-muted-foreground text-sm">
              {emojis.length === 0 ? 'This server has no custom emoji.' : 'No emoji match.'}
            </p>
          )}
        </div>
      </PopoverContent>
    </Popover>
  )
}
