import { useEffect, useMemo, useRef, useState, type ReactElement } from 'react'
import {
  Apple,
  Car,
  Clock,
  Flag,
  Hash,
  Lightbulb,
  PawPrint,
  Smile,
  Sparkles,
  Trophy,
  type LucideIcon,
} from 'lucide-react'

import type { mastodon } from '../masto.ts'
import { useCustomEmojis } from '@/hooks/use-custom-emojis.ts'
import { useUnicodeEmojis } from '@/hooks/use-unicode-emojis.ts'
import { searchEmojis, type AnyEmoji, type UnicodeEmoji } from '@/lib/emoji-search.ts'
import { frequentEmojiKeys, recordEmojiUse } from '@/lib/frequent-emoji.ts'
import { useReadingPreferences } from '../reading-preferences.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover.tsx'

/** What was picked: a custom emoji's shortcode, or a Unicode emoji's text. */
export type PickedEmoji = { type: 'custom'; shortcode: string } | { type: 'unicode'; native: string }

// The key `recordEmojiUse` counts a pick under.
export const emojiKey = (picked: PickedEmoji) =>
  picked.type === 'custom' ? `:${picked.shortcode}:` : picked.native

function CustomEmojiButton({
  emoji,
  onPick,
}: {
  emoji: mastodon.v1.CustomEmoji
  onPick: () => void
}) {
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

function UnicodeEmojiButton({ emoji, onPick }: { emoji: UnicodeEmoji; onPick: () => void }) {
  const name = `:${emoji.shortcodes[0] ?? emoji.id}:`
  return (
    <button
      type="button"
      title={name}
      aria-label={`${emoji.native} ${name}`}
      onClick={onPick}
      className="hover:bg-accent focus-visible:ring-ring flex size-8 items-center justify-center rounded-md text-xl leading-none outline-none focus-visible:ring-2"
    >
      {emoji.native}
    </button>
  )
}

// Mastodon's picker categories after its custom ones, with emoji-mart's names,
// over Unicode's groups; "people" is emoji-mart's Smileys & People.
const UNICODE_CATEGORIES: { id: string; title: string; groups: number[]; icon: LucideIcon }[] = [
  { id: 'people', title: 'People', groups: [0, 1], icon: Smile },
  { id: 'nature', title: 'Nature', groups: [3], icon: PawPrint },
  { id: 'foods', title: 'Food & Drink', groups: [4], icon: Apple },
  { id: 'activity', title: 'Activity', groups: [6], icon: Trophy },
  { id: 'places', title: 'Travel & Places', groups: [5], icon: Car },
  { id: 'objects', title: 'Objects', groups: [7], icon: Lightbulb },
  { id: 'symbols', title: 'Symbols', groups: [8], icon: Hash },
  { id: 'flags', title: 'Flags', groups: [9], icon: Flag },
]

interface Section {
  id: string
  title: string
  icon: LucideIcon
  emojis: AnyEmoji[]
}

/**
 * Mastodon's emoji picker: what the reader uses most, then the server's custom
 * emoji by category, then the Unicode emoji by category, with a search over
 * both. `onPick` is handed what was picked.
 */
export function EmojiPicker({
  onPick,
  trigger,
  label = 'Insert emoji',
}: {
  onPick: (emoji: PickedEmoji) => void
  /** The button that opens it; an icon button by default. */
  trigger?: ReactElement
  label?: string
}) {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const custom = useCustomEmojis(open)
  const unicode = useUnicodeEmojis(open)
  const listRef = useRef<HTMLDivElement>(null)
  // Read when the picker opens, so a pick does not reshuffle the row under the
  // pointer.
  const [frequentKeys, setFrequentKeys] = useState<string[]>([])
  useEffect(() => {
    if (!open || !unicode) return
    setFrequentKeys(
      frequentEmojiKeys(
        (shortcode) => unicode.find((e) => e.shortcodes.includes(shortcode))?.native ?? null,
      ),
    )
  }, [open, unicode])

  const sections = useMemo<Section[]>(() => {
    const trimmed = query.trim()
    if (trimmed) {
      const results = searchEmojis(unicode ?? [], custom, trimmed)
      return results.length > 0
        ? [{ id: 'search', title: 'Search results', icon: Sparkles, emojis: results }]
        : []
    }
    const byNative = new Map((unicode ?? []).map((e) => [e.native, e]))
    const byShortcode = new Map(custom.map((e) => [e.shortcode, e]))
    const frequent: AnyEmoji[] = frequentKeys.flatMap((key): AnyEmoji[] => {
      const customEmoji = key.startsWith(':') ? byShortcode.get(key.slice(1, -1)) : undefined
      if (customEmoji) return [{ type: 'custom', emoji: customEmoji }]
      const unicodeEmoji = byNative.get(key)
      return unicodeEmoji ? [{ type: 'unicode', emoji: unicodeEmoji }] : []
    })
    const byCategory = new Map<string, mastodon.v1.CustomEmoji[]>()
    for (const emoji of custom) {
      const category = emoji.category ?? ''
      byCategory.set(category, [...(byCategory.get(category) ?? []), emoji])
    }
    // Uncategorized first, as "Custom", then the categories by name.
    const customSections: Section[] = [...byCategory.entries()]
      .sort(([a], [b]) => (a === '' ? -1 : b === '' ? 1 : a.localeCompare(b)))
      .map(([category, list]) => ({
        id: `custom-${category}`,
        title: category || 'Custom',
        icon: Sparkles,
        emojis: list.map((emoji) => ({ type: 'custom', emoji }) as const),
      }))
    return [
      ...(frequent.length > 0
        ? [{ id: 'recent', title: 'Frequently used', icon: Clock, emojis: frequent }]
        : []),
      ...customSections,
      ...UNICODE_CATEGORIES.map((c) => ({
        id: c.id,
        title: c.title,
        icon: c.icon,
        emojis: (unicode ?? [])
          .filter((e) => c.groups.includes(e.group))
          .map((emoji) => ({ type: 'unicode', emoji }) as const),
      })),
    ].filter((s) => s.emojis.length > 0)
  }, [query, unicode, custom, frequentKeys])

  const pick = (emoji: AnyEmoji) => {
    const picked: PickedEmoji =
      emoji.type === 'custom'
        ? { type: 'custom', shortcode: emoji.emoji.shortcode }
        : { type: 'unicode', native: emoji.emoji.native }
    recordEmojiUse(emojiKey(picked))
    onPick(picked)
    setOpen(false)
    setQuery('')
  }

  const searching = query.trim() !== ''

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
        {!searching && sections.length > 1 && (
          <nav aria-label="Emoji categories" className="flex flex-wrap gap-0.5">
            {sections.map((s) => (
              <Button
                key={s.id}
                type="button"
                variant="ghost"
                size="icon-xs"
                title={s.title}
                aria-label={s.title}
                onClick={() =>
                  listRef.current
                    ?.querySelector(`[data-section="${CSS.escape(s.id)}"]`)
                    ?.scrollIntoView({ block: 'start' })
                }
              >
                <s.icon />
              </Button>
            ))}
          </nav>
        )}
        <Input
          type="search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search..."
          aria-label="Search emoji"
          className="h-8"
        />
        <div ref={listRef} className="max-h-64 space-y-2 overflow-y-auto">
          {sections.map((s) => (
            <section key={s.id} data-section={s.id} aria-label={s.title}>
              <h3 className="text-muted-foreground bg-popover sticky top-0 mb-1 text-xs font-medium">
                {s.title}
              </h3>
              <div className="grid grid-cols-8 gap-0.5">
                {s.emojis.map((emoji) =>
                  emoji.type === 'custom' ? (
                    <CustomEmojiButton
                      key={`:${emoji.emoji.shortcode}`}
                      emoji={emoji.emoji}
                      onPick={() => pick(emoji)}
                    />
                  ) : (
                    <UnicodeEmojiButton
                      key={emoji.emoji.hexcode}
                      emoji={emoji.emoji}
                      onPick={() => pick(emoji)}
                    />
                  ),
                )}
              </div>
            </section>
          ))}
          {searching && sections.length === 0 && (
            <p className="text-muted-foreground text-sm">No matching emojis found</p>
          )}
          {!searching && !unicode && (
            <p className="text-muted-foreground text-sm">Loading…</p>
          )}
        </div>
      </PopoverContent>
    </Popover>
  )
}
