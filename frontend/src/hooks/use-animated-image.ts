import { useState } from 'react'

import { useReadingPreferences } from '../reading-preferences.ts'

/**
 * Which of an image's two files to show, as Mastodon's `Avatar` does with
 * `useHovering(autoPlayGif)`: the animated one when GIFs auto-play, and
 * otherwise the still one until the pointer is over it. Spread `hover` on the
 * element that should start the animation.
 */
export function useAnimatedImage(
  animated: string | null | undefined,
  still: string | null | undefined,
) {
  const { autoPlayGif } = useReadingPreferences()
  const [hovering, setHovering] = useState(false)
  const src = autoPlayGif || hovering ? animated || still : still || animated
  return {
    src: src || undefined,
    hover: autoPlayGif
      ? {}
      : {
          onMouseEnter: () => setHovering(true),
          onMouseLeave: () => setHovering(false),
        },
  }
}

/** A header plays only when GIFs auto-play (`account_header`); it has no hover. */
export function useAnimatedHeader(
  animated: string | null | undefined,
  still: string | null | undefined,
): string | undefined {
  const { autoPlayGif } = useReadingPreferences()
  return (autoPlayGif ? animated || still : still || animated) || undefined
}
