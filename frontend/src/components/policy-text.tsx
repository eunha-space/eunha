import { cn } from '@/lib/utils.ts'

/**
 * A policy document the server rendered from Markdown: the terms of service,
 * the privacy policy, a changelog.
 *
 * The HTML is safe to insert: the server escapes any HTML in the source and
 * renders no images (Mastodon's Redcarpet settings, `escape_html` and
 * `no_images`), so what arrives is only the tags Markdown itself produces.
 */
export function PolicyText({ html, className }: { html: string; className?: string }) {
  return (
    <div
      className={cn(
        'space-y-3 text-sm leading-relaxed break-words',
        '[&_h1]:text-xl [&_h1]:font-bold [&_h2]:pt-2 [&_h2]:text-lg [&_h2]:font-semibold',
        '[&_h3]:font-semibold [&_h4]:font-semibold',
        '[&_ul]:list-disc [&_ul]:space-y-1 [&_ul]:pl-5 [&_ol]:list-decimal [&_ol]:space-y-1 [&_ol]:pl-5',
        '[&_a]:text-primary [&_a]:underline [&_blockquote]:border-l-2 [&_blockquote]:pl-3',
        '[&_code]:bg-muted [&_code]:rounded [&_code]:px-1 [&_pre]:overflow-x-auto',
        className,
      )}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  )
}
