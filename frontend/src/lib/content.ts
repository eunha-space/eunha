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
