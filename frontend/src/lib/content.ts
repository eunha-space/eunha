// A local post that quotes another carries a `<p class="quote-inline">RE: …`
// link for clients that cannot show the quote. Mastodon's web client takes it
// out of a post whose quote it shows (`stripQuoteFallback` in its status
// importer), and so does this one.
export function stripQuoteFallback(html: string): string {
  if (!html.includes('quote-inline')) return html
  const wrapper = document.createElement('div')
  wrapper.innerHTML = html
  wrapper.querySelector('.quote-inline')?.remove()
  return wrapper.innerHTML
}

/**
 * The start of `html`, cut after `limit` characters of its text, and whether
 * anything was cut. Only that much is ever parsed: the markup is sliced
 * first, at a length no text of `limit` characters can need more than, so a
 * post of a megabyte costs what one of a few pages does. The parser closes
 * what the slice leaves open and drops a tag it cuts in two.
 */
export function truncateHtml(html: string, limit: number): { html: string; truncated: boolean } {
  if (html.length <= limit) return { html, truncated: false }
  // Markup around each character of text, generously: links, `<br>`s.
  const sliced = html.length > limit * 8 ? html.slice(0, limit * 8) : html
  const template = document.createElement('template')
  template.innerHTML = sliced
  const walker = document.createTreeWalker(template.content, NodeFilter.SHOW_TEXT)
  let left = limit
  let cut: Text | null = null
  for (let node = walker.nextNode() as Text | null; node; node = walker.nextNode() as Text | null) {
    if (node.data.length >= left) {
      cut = node
      break
    }
    left -= node.data.length
  }
  if (!cut) return { html: template.innerHTML, truncated: sliced !== html }
  cut.data = cut.data.slice(0, left)
  // Drop everything after the cut, at every level up to the root.
  for (let node: Node | null = cut; node && node !== template.content; node = node.parentNode) {
    while (node.nextSibling) node.nextSibling.remove()
  }
  return { html: template.innerHTML, truncated: true }
}
