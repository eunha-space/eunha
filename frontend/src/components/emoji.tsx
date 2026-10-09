// Custom emoji, drawn as Mastodon 4.7's web client draws them
// (`components/emoji`): a `:shortcode:` in a text node becomes an image only
// when the entity it belongs to lists that shortcode in its `emojis`, and
// stays text otherwise. The image is the still file unless GIFs auto-play or
// the pointer is over the element that animates it (`AnimateEmojiProvider`),
// and its `alt` and `title` are the shortcode.
//
// Text is turned into React nodes, so nothing in it is ever parsed as markup.
// HTML (a post, a bio, a field's value) is server-sanitized; it is parsed in
// an inert `<template>`, only its text nodes are touched, and each image is
// built with DOM calls, so a shortcode or URL lands in an attribute escaped and
// never as markup of its own.
import {
  createContext,
  useContext,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from 'react'

import { useReadingPreferences } from '../reading-preferences.ts'

/** A custom emoji as masto.js (camelCase) or a raw API response gives it. */
export interface EmojiLike {
  shortcode: string
  url: string
  staticUrl?: string | null
  static_url?: string | null
}

export type EmojiList = readonly EmojiLike[] | null | undefined

interface EmojiFiles {
  url: string
  still: string
}

type EmojiMap = ReadonlyMap<string, EmojiFiles>

// Mastodon's `CUSTOM_EMOJI_REGEX`, taken from left to right: a match that is
// not one of the entity's emoji is left as text and not looked into again.
const SHORTCODE = /:([a-z0-9_]+):/gi

// An image is only ever fetched from the web; anything else is not drawn.
function webUrl(value: string | null | undefined): string | null {
  if (!value) return null
  try {
    const url = new URL(value, window.location.href)
    return url.protocol === 'https:' || url.protocol === 'http:' ? url.href : null
  } catch {
    return null
  }
}

// The same emoji arrive as a new array each time their entity is fetched
// again; keyed on what they say, the HTML built from them stays the same
// string, and React leaves the element (and the pointer over it) alone.
function useEmojiMap(emojis: EmojiList): EmojiMap {
  const key = JSON.stringify(
    (emojis ?? []).map((e) => [e.shortcode, e.url, e.staticUrl ?? e.static_url]),
  )
  // eslint-disable-next-line react-hooks/exhaustive-deps
  return useMemo(() => toMap(emojis), [key])
}

function toMap(emojis: EmojiList): EmojiMap {
  const map = new Map<string, EmojiFiles>()
  for (const emoji of emojis ?? []) {
    const url = webUrl(emoji.url)
    const still = webUrl(emoji.staticUrl ?? emoji.static_url) ?? url
    if (!url || !still || !/^[a-z0-9_]+$/i.test(emoji.shortcode)) continue
    map.set(emoji.shortcode, { url, still })
  }
  return map
}

// Split a text into its runs of text and the emoji among them.
function tokenize(text: string, map: EmojiMap): (string | { code: string; files: EmojiFiles })[] {
  if (map.size === 0 || !text.includes(':')) return [text]
  const tokens: (string | { code: string; files: EmojiFiles })[] = []
  let last = 0
  for (const match of text.matchAll(SHORTCODE)) {
    const files = map.get(match[1])
    if (!files) continue
    if (match.index > last) tokens.push(text.slice(last, match.index))
    tokens.push({ code: match[1], files })
    last = match.index + match[0].length
  }
  if (last === 0) return [text]
  if (last < text.length) tokens.push(text.slice(last))
  return tokens
}

const EMOJI_CLASS = 'emojione custom-emoji'

/** Whether the emoji inside an element animate: `null` outside any. */
const AnimateEmojiContext = createContext<boolean | null>(null)

function useAnimateState() {
  const parent = useContext(AnimateEmojiContext)
  const { autoPlayGif } = useReadingPreferences()
  const [hovering, setHovering] = useState(false)
  const own = parent === null
  return {
    animate: own ? autoPlayGif || hovering : parent,
    autoPlayGif,
    // Inside another, the outer element decides, as Mastodon's provider does.
    handlers:
      own && !autoPlayGif
        ? {
            onMouseEnter: () => setHovering(true),
            onMouseLeave: () => setHovering(false),
          }
        : {},
    own,
  }
}

type Tag = 'div' | 'span' | 'section' | 'header' | 'strong' | 'dd' | 'dt' | 'li' | 'p'

type ProviderProps = ComponentPropsWithoutRef<'div'> & { as?: Tag }

/**
 * Mastodon's `AnimateEmojiProvider`: an element whose custom emoji, everywhere
 * inside it, animate while the pointer is over it.
 */
export function AnimateEmoji({ as = 'div', children, onMouseEnter, onMouseLeave, ...props }: ProviderProps) {
  const { animate, handlers, own } = useAnimateState()
  const Wrapper = as as 'div'
  if (!own) return <Wrapper {...props} onMouseEnter={onMouseEnter} onMouseLeave={onMouseLeave}>{children}</Wrapper>
  return (
    <Wrapper
      {...props}
      onMouseEnter={(event) => {
        onMouseEnter?.(event)
        handlers.onMouseEnter?.()
      }}
      onMouseLeave={(event) => {
        onMouseLeave?.(event)
        handlers.onMouseLeave?.()
      }}
    >
      <AnimateEmojiContext.Provider value={animate}>{children}</AnimateEmojiContext.Provider>
    </Wrapper>
  )
}

/**
 * Plain text — a display name, a content warning, a poll option, a field's
 * name — with its custom emoji drawn. Animates with the element around it, or
 * by the auto-play setting alone outside one.
 */
export function EmojiText({ text, emojis }: { text: string; emojis: EmojiList }) {
  const parent = useContext(AnimateEmojiContext)
  const { autoPlayGif } = useReadingPreferences()
  const animate = parent ?? autoPlayGif
  const map = useEmojiMap(emojis)
  const tokens = useMemo(() => tokenize(text, map), [text, map])
  if (tokens.length === 1 && typeof tokens[0] === 'string') return <>{tokens[0]}</>
  return (
    <>
      {tokens.map((token, i): ReactNode =>
        typeof token === 'string' ? (
          token
        ) : (
          <img
            key={i}
            src={animate ? token.files.url : token.files.still}
            alt={`:${token.code}:`}
            title={`:${token.code}:`}
            className={EMOJI_CLASS}
            loading="lazy"
            draggable={false}
          />
        ),
      )}
    </>
  )
}

/** `html` with every listed `:shortcode:` in its text replaced by an image. */
export function emojifyHtml(html: string, emojis: EmojiList | EmojiMap, animate: boolean): string {
  const map = emojis instanceof Map ? (emojis as EmojiMap) : toMap(emojis as EmojiList)
  if (map.size === 0 || !html.includes(':')) return html
  const template = document.createElement('template')
  template.innerHTML = html
  const doc = template.content.ownerDocument
  const walker = doc.createTreeWalker(template.content, NodeFilter.SHOW_TEXT)
  const texts: Text[] = []
  for (let node = walker.nextNode(); node; node = walker.nextNode()) texts.push(node as Text)
  let changed = false
  for (const node of texts) {
    const tokens = tokenize(node.data, map)
    if (tokens.length === 1 && typeof tokens[0] === 'string') continue
    changed = true
    const fragment = doc.createDocumentFragment()
    for (const token of tokens) {
      if (typeof token === 'string') {
        fragment.append(doc.createTextNode(token))
        continue
      }
      const img = doc.createElement('img')
      img.setAttribute('src', animate ? token.files.url : token.files.still)
      img.setAttribute('alt', `:${token.code}:`)
      img.setAttribute('title', `:${token.code}:`)
      img.setAttribute('class', EMOJI_CLASS)
      img.setAttribute('loading', 'lazy')
      img.setAttribute('draggable', 'false')
      img.dataset.original = token.files.url
      img.dataset.static = token.files.still
      fragment.append(img)
    }
    node.replaceWith(fragment)
  }
  return changed ? template.innerHTML : html
}

type HtmlProps = Omit<ComponentPropsWithoutRef<'div'>, 'children' | 'dangerouslySetInnerHTML'> & {
  html: string
  emojis: EmojiList
  as?: Tag
}

/**
 * Mastodon's `EmojiHTML`: server-sanitized HTML with its custom emoji drawn.
 * It animates them on its own hover, unless it sits inside an element that
 * already does.
 */
export function EmojiHtml({ html, emojis, as = 'div', onMouseEnter, onMouseLeave, ...props }: HtmlProps) {
  const { animate, autoPlayGif, handlers } = useAnimateState()
  const map = useEmojiMap(emojis)
  // Built with the files auto-play asks for, so a page that animates them
  // does not first fetch the still ones; hovering only swaps the `src`s
  // below, and leaves the rest of the element (a selection, say) alone.
  //
  // The object is kept, not just its string: React writes `innerHTML` again
  // for a new one, which would replace the node under the pointer while it
  // hovers, and the browser then never says the pointer left.
  const inner = useMemo(
    () => ({ __html: emojifyHtml(html, map, autoPlayGif) }),
    [html, map, autoPlayGif],
  )
  const ref = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    for (const img of ref.current?.querySelectorAll<HTMLImageElement>('img.custom-emoji') ?? []) {
      const src = animate ? img.dataset.original : img.dataset.static
      if (src && img.getAttribute('src') !== src) img.setAttribute('src', src)
    }
  }, [animate, inner])
  const Element = as as 'div'
  return (
    <Element
      {...props}
      ref={ref}
      onMouseEnter={(event) => {
        onMouseEnter?.(event)
        handlers.onMouseEnter?.()
      }}
      onMouseLeave={(event) => {
        onMouseLeave?.(event)
        handlers.onMouseLeave?.()
      }}
      dangerouslySetInnerHTML={inner}
    />
  )
}

/**
 * An account's display name with its emoji, falling back to the username as
 * Mastodon's `display_name_html` does. It animates on its own hover, like
 * Mastodon's `DisplayName`, unless it sits in an element that does.
 */
export function DisplayName({
  account,
  className,
}: {
  account: { displayName?: string; display_name?: string; username: string; emojis?: EmojiList }
  className?: string
}) {
  const name = (account.displayName ?? account.display_name) || account.username
  return (
    <AnimateEmoji as="span" className={className}>
      <EmojiText text={name} emojis={account.emojis} />
    </AnimateEmoji>
  )
}
