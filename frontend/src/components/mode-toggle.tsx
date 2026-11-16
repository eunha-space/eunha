import { Laptop, Moon, Sun, SunMoon } from 'lucide-react'
import { defineMessages, useIntl } from 'react-intl'

import {
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
} from '@/components/ui/dropdown-menu.tsx'
import { ToggleGroup, ToggleGroupItem } from '@/components/ui/toggle-group.tsx'
import { useTheme } from '@/components/theme-provider.tsx'

const messages = defineMessages({
  theme: { id: 'eunha.theme.label', defaultMessage: 'Theme' },
  light: { id: 'mastodon.color_scheme.light', defaultMessage: 'Light' },
  dark: { id: 'mastodon.color_scheme.dark', defaultMessage: 'Dark' },
  system: { id: 'eunha.theme.system', defaultMessage: 'System' },
})

const themes = [
  { value: 'light', label: messages.light, icon: Sun },
  { value: 'dark', label: messages.dark, icon: Moon },
  { value: 'system', label: messages.system, icon: Laptop },
] as const

type Theme = (typeof themes)[number]['value']

const isTheme = (value: unknown): value is Theme =>
  themes.some((theme) => theme.value === value)

// Light, Dark and System as one pill, the chosen one filled, cornered like
// the account menu rather than rounded off. Signed out, it
// sits beside the sign-in button; signed in, `ThemeMenuRow` puts the same
// choice in the account menu.
export function ModeToggle() {
  const { theme, setTheme } = useTheme()
  const intl = useIntl()

  return (
    <ToggleGroup
      aria-label={intl.formatMessage(messages.theme)}
      size="sm"
      spacing={0}
      value={[theme]}
      onValueChange={(value) => {
        // Pressing the chosen segment again would empty the group; keep it.
        const next = value.find((v) => v !== theme) ?? value[0]
        if (isTheme(next)) setTheme(next)
      }}
      className="rounded-lg! border p-0.5"
    >
      {themes.map(({ value, label, icon: Icon }) => (
        <ToggleGroupItem
          key={value}
          value={value}
          aria-label={intl.formatMessage(label)}
          title={intl.formatMessage(label)}
          className="rounded-md! px-2.5 aria-pressed:bg-muted"
        >
          <Icon />
        </ToggleGroupItem>
      ))}
    </ToggleGroup>
  )
}

// The theme as a row of the account menu, shaped like the Language row
// beside it: an icon and a label, with the three choices as a small icon
// switch at its end. The choices are menu items, so the menu's arrow keys
// reach them, and choosing one leaves the menu open, showing the new choice.
export function ThemeMenuRow() {
  const { theme, setTheme } = useTheme()
  const intl = useIntl()

  return (
    <div className="theme-menu-row flex items-center gap-2.5 text-sm">
      <SunMoon className="size-4 shrink-0" aria-hidden />
      <span className="flex-1">{intl.formatMessage(messages.theme)}</span>
      <DropdownMenuRadioGroup
        aria-label={intl.formatMessage(messages.theme)}
        value={theme}
        onValueChange={(value) => {
          if (isTheme(value)) setTheme(value)
        }}
        className="flex gap-0.5 rounded-md border p-0.5"
      >
        {themes.map(({ value, label, icon: Icon }) => (
          <DropdownMenuRadioItem
            key={value}
            value={value}
            closeOnClick={false}
            aria-label={intl.formatMessage(label)}
            title={intl.formatMessage(label)}
            className="text-muted-foreground data-checked:bg-muted data-checked:text-foreground h-6 w-7 justify-center rounded-sm! p-0! [&>[data-slot=dropdown-menu-radio-item-indicator]]:hidden"
          >
            <Icon className="size-3.5" />
          </DropdownMenuRadioItem>
        ))}
      </DropdownMenuRadioGroup>
    </div>
  )
}
