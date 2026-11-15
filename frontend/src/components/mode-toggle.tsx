import { Laptop, Moon, Sun } from 'lucide-react'

import {
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
} from '@/components/ui/dropdown-menu.tsx'
import { ToggleGroup, ToggleGroupItem } from '@/components/ui/toggle-group.tsx'
import { useTheme } from '@/components/theme-provider.tsx'

const themes = [
  { value: 'light', label: 'Light', icon: Sun },
  { value: 'dark', label: 'Dark', icon: Moon },
  { value: 'system', label: 'System', icon: Laptop },
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

  return (
    <ToggleGroup
      aria-label="Theme"
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
          aria-label={label}
          title={label}
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

  return (
    <DropdownMenuRadioGroup
      aria-label="Theme"
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
          {label}
        </DropdownMenuRadioItem>
      ))}
    </DropdownMenuRadioGroup>
  )
}
