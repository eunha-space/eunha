import { Languages } from 'lucide-react'
import { defineMessages, useIntl } from 'react-intl'
import { toast } from 'sonner'

import { Button } from '@/components/ui/button.tsx'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu.tsx'
import { LOCALES, useLocale, type Locale } from '@/i18n.tsx'

const messages = defineMessages({
  language: { id: 'mastodon.admin.statuses.language', defaultMessage: 'Language' },
  saveFailed: {
    id: 'eunha.language.save_failed',
    defaultMessage: 'Could not save the language to your account',
  },
})

// Each language is named in itself, so it can be found from any other.
function LanguageItems() {
  const intl = useIntl()
  const { locale, setLocale } = useLocale()
  const choose = (code: Locale) =>
    setLocale(code).catch(() => toast.error(intl.formatMessage(messages.saveFailed)))
  return (
    <DropdownMenuRadioGroup
      value={locale}
      onValueChange={(code) => {
        if (LOCALES.some((l) => l.code === code)) void choose(code as Locale)
      }}
    >
      {LOCALES.map(({ code, name }) => (
        <DropdownMenuRadioItem key={code} value={code} lang={code} closeOnClick>
          {name}
        </DropdownMenuRadioItem>
      ))}
    </DropdownMenuRadioGroup>
  )
}

// In the account menu, beside the theme pill: the interface language, which
// is the account's own and so follows it to other browsers, and its mail.
export function LanguageSubmenu() {
  const intl = useIntl()
  const { locale } = useLocale()
  const current = LOCALES.find((l) => l.code === locale)?.name
  return (
    <DropdownMenuSub>
      <DropdownMenuSubTrigger>
        <Languages />
        <span className="flex-1">
          {intl.formatMessage(messages.language)}
        </span>
        <span className="text-muted-foreground text-xs">{current}</span>
      </DropdownMenuSubTrigger>
      <DropdownMenuSubContent>
        <LanguageItems />
      </DropdownMenuSubContent>
    </DropdownMenuSub>
  )
}

// Signed out, beside the theme pill: the same choice, kept in this browser.
export function LanguageButton() {
  const intl = useIntl()
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={
          <Button
            variant="outline"
            size="icon-sm"
            aria-label={intl.formatMessage(messages.language)}
            title={intl.formatMessage(messages.language)}
          />
        }
      >
        <Languages />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" side="top" className="w-auto">
        <LanguageItems />
      </DropdownMenuContent>
    </DropdownMenu>
  )
}
