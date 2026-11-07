import { createContext, useContext, useEffect, useState } from 'react'

type Theme = 'dark' | 'light' | 'system'

export const PALETTES = {
  mint: 'Mint',
  lilac: 'Lilac',
  ocean: 'Ocean',
  rose: 'Rose',
  amber: 'Amber',
} as const

export type Palette = keyof typeof PALETTES

type ThemeProviderProps = {
  children: React.ReactNode
  defaultTheme?: Theme
  storageKey?: string
}

type ThemeProviderState = {
  theme: Theme
  palette: Palette
  setPalette: (palette: Palette) => void
  setTheme: (theme: Theme) => void
}

const initialState: ThemeProviderState = {
  theme: 'system',
  palette: 'mint',
  setPalette: () => null,
  setTheme: () => null,
}

const ThemeProviderContext = createContext<ThemeProviderState>(initialState)

export function ThemeProvider({
  children,
  defaultTheme = 'system',
  storageKey = 'eunha-theme',
  ...props
}: ThemeProviderProps) {
  const [theme, setTheme] = useState<Theme>(
    () => (localStorage.getItem(storageKey) as Theme) || defaultTheme,
  )

  const paletteKey = `${storageKey}-palette`
  const [palette, setPalette] = useState<Palette>(() => {
    const saved = localStorage.getItem(paletteKey)
    return saved && Object.hasOwn(PALETTES, saved) ? saved as Palette : 'mint'
  })

  useEffect(() => {
    document.documentElement.dataset.palette = palette
  }, [palette])

  useEffect(() => {
    const root = window.document.documentElement
    const mq = window.matchMedia('(prefers-color-scheme: dark)')

    const apply = () => {
      root.classList.remove('light', 'dark')
      const resolved =
        theme === 'system' ? (mq.matches ? 'dark' : 'light') : theme
      root.classList.add(resolved)
    }

    apply()

    // While following the system, keep up with OS changes live.
    if (theme === 'system') {
      mq.addEventListener('change', apply)
      return () => mq.removeEventListener('change', apply)
    }
  }, [theme])

  const value = {
    theme,
    palette,
    setPalette: (palette: Palette) => {
      localStorage.setItem(paletteKey, palette)
      setPalette(palette)
    },
    setTheme: (theme: Theme) => {
      localStorage.setItem(storageKey, theme)
      setTheme(theme)
    },
  }

  return (
    <ThemeProviderContext.Provider {...props} value={value}>
      {children}
    </ThemeProviderContext.Provider>
  )
}

// eslint-disable-next-line react-refresh/only-export-components
export const useTheme = () => {
  const context = useContext(ThemeProviderContext)
  if (context === undefined)
    throw new Error('useTheme must be used within a ThemeProvider')
  return context
}
