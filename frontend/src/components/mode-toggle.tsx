import { Laptop, Moon, Sun } from 'lucide-react'
import { defineMessages, useIntl } from 'react-intl'

import {
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
} from '@/components/ui/dropdown-menu.tsx'
import { ToggleGroup, ToggleGroupItem } from '@/components/ui/toggle-group.tsx'
import { useTheme } from '@/components/theme-provider.tsx'

const messages = defineMessages({
  theme: { id: 'eunha.theme.label', defaultMessage: 'Theme' },
  light: { id: 'eunha.theme.light', defaultMessage: 'Light' },
  dark: { id: 'eunha.theme.dark', defaultMessage: 'Dark' },
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
// sits beside the sign-in button; signed in, `ThemeMenuPill` puts the same
// pill in the account menu.
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

// The same pill as menu items, words only so it fits the card-wide menu, so the account menu's arrow keys reach every
// segment. Choosing one leaves the menu open, showing the new choice.
export function ThemeMenuPill() {
  const { theme, setTheme } = useTheme()
  const intl = useIntl()

  return (
    <DropdownMenuRadioGroup
      aria-label={intl.formatMessage(messages.theme)}
      value={theme}
      onValueChange={(value) => {
        if (isTheme(value)) setTheme(value)
      }}
      className="my-1 flex gap-0.5 rounded-lg border p-0.5"
    >
      {themes.map(({ value, label }) => (
        <DropdownMenuRadioItem
          key={value}
          value={value}
          closeOnClick={false}
          className="min-w-0 flex-1 justify-center rounded-md! px-1.5! text-xs data-checked:bg-muted data-checked:font-medium [&>[data-slot=dropdown-menu-radio-item-indicator]]:hidden"
        >
          {intl.formatMessage(label)}
        </DropdownMenuRadioItem>
      ))}
    </DropdownMenuRadioGroup>
  )
}
