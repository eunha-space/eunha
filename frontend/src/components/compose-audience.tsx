import { useState } from 'react'
import { MessageCircle, Newspaper, Search } from 'lucide-react'

import type { mastodon } from '../masto.ts'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu.tsx'
import { t } from '@/i18n.tsx'

type Visibility = mastodon.v1.StatusVisibility

// The shadcn wrapper puts a radio's indicator on the right. 5.0 leads with the
// checkmark, so it swaps sides here rather than in the shared primitive.
const leadingCheck =
  'pl-8 pr-2 [&_[data-slot=dropdown-menu-radio-item-indicator]]:left-2 [&_[data-slot=dropdown-menu-radio-item-indicator]]:right-auto'

// Handles as they are written in a post. Mastodon's own pattern, minus the
// lookahead cases that only matter inside rendered HTML.
const MENTION = /@([a-z0-9_]+(?:@[\w.-]+\w)?)/gi

/**
 * Who this post will reach, in the order the label names them.
 *
 * A reply always leads with the account replied to, and keeps leading with it
 * even after its @-mention is deleted from the text — that is the whole point
 * of the label. Mastodon 5.0 does the same: the consequence being surfaced is
 * that a mentioned account sees a followers-only post whether or not it
 * follows you, and deleting the visible mention does not change that.
 */
export function audience(
  text: string,
  replyTo: mastodon.v1.Status | null | undefined,
  meAcct: string | null,
): string[] {
  const seen = new Set<string>()
  const out: string[] = []
  const add = (acct: string) => {
    const key = acct.toLowerCase()
    if (!acct || key === meAcct?.toLowerCase() || seen.has(key)) return
    seen.add(key)
    out.push(acct)
  }
  if (replyTo) add(replyTo.account.acct)
  for (const m of text.matchAll(MENTION)) add(m[1])
  return out
}

// A menu row carrying a switch rather than a checkmark — the shape 5.0 gives
// the two settings that qualify a public post. The menu stays open, because
// these are adjustments to a choice already made rather than the choice.
function SwitchItem({
  icon,
  label,
  checked,
  disabled,
  onChange,
}: {
  icon: React.ReactNode
  label: string
  checked: boolean
  disabled?: boolean
  onChange: (on: boolean) => void
}) {
  return (
    <DropdownMenuItem
      closeOnClick={false}
      disabled={disabled}
      onClick={() => !disabled && onChange(!checked)}
      className="gap-2 py-2"
    >
      {icon}
      <span className="flex-1 pr-2 text-xs leading-snug">{label}</span>
      <Switch size="sm" checked={checked} disabled={disabled} tabIndex={-1} />
    </DropdownMenuItem>
  )
}

function Trigger({
  visibility,
  people,
  replyTo,
}: {
  visibility: Visibility
  people: string[]
  replyTo: mastodon.v1.Status | null | undefined
}) {
  if (visibility === 'public') return <>{t({ id: 'privacy.public.short', defaultMessage: 'Public' })}</>
  if (visibility === 'unlisted') return <>{t({ id: 'privacy.unlisted.short', defaultMessage: 'Quiet public' })}</>

  // A reply names the account replied to by display name, because that account
  // is in hand; a handle typed into the box is only ever a handle.
  const lead =
    replyTo && people[0] === replyTo.account.acct
      ? replyTo.account.displayName || replyTo.account.username
      : people[0] && `@${people[0]}`

  if (visibility === 'private') {
    const extra = people.length
    return (
      <>
        {lead
          ? t({ id: 'eunha.compose_audience.lead_and_followers', defaultMessage: '{lead}, Your followers' }, { lead })
          : t({ id: 'eunha.compose_audience.your_followers', defaultMessage: 'Your followers' })}
        {extra > 1
          ? t({ id: 'eunha.compose_audience.and_others', defaultMessage: '{count, plural, one { + # other} other { + # others}}' }, { count: extra - 1 })
          : ''}
      </>
    )
  }
  // Direct: the people *are* the audience, so there is nothing else to name.
  if (!lead) return <>{t({ id: 'eunha.compose_audience.nobody_yet', defaultMessage: 'Nobody yet' })}</>
  const others = people.length - 1
  return (
    <>
      {lead}
      {others > 0
        ? t({ id: 'eunha.compose_audience.and_others', defaultMessage: '{count, plural, one { + # other} other { + # others}}' }, { count: others })
        : ''}
    </>
  )
}

export function ComposeAudience({
  visibility,
  onVisibilityChange,
  text,
  replyTo,
  meAcct,
  defaultVisibility,
  disabled,
}: {
  visibility: Visibility
  onVisibilityChange: (v: Visibility) => void
  text: string
  replyTo?: mastodon.v1.Status | null
  meAcct: string | null
  defaultVisibility: Visibility
  disabled?: boolean
}) {
  const [confirmingPost, setConfirmingPost] = useState(false)
  const people = audience(text, replyTo, meAcct)
  const isMessage = visibility === 'direct'
  const isFollowers = visibility === 'private'

  // "Public" and "quiet public" are one choice plus a switch. Unchecking
  // discoverability is the only way to reach `unlisted`, which is what upstream
  // means by taking quiet public out of the visibility list.
  const setDiscoverable = (on: boolean) => onVisibilityChange(on ? 'public' : 'unlisted')

  return (
    <>
      <span className="text-muted-foreground text-xs">{t({ id: 'eunha.compose_audience.to', defaultMessage: 'To:' })}</span>
      <DropdownMenu>
        <DropdownMenuTrigger
          disabled={disabled}
          aria-label={t({ id: 'eunha.compose_audience.who_can_see_this', defaultMessage: 'Who can see this' })}
          className="bg-muted hover:bg-muted/70 data-[popup-open]:bg-foreground data-[popup-open]:text-background flex max-w-[16rem] items-center gap-1 rounded-full px-3 py-1 text-xs font-medium disabled:opacity-50"
        >
          <span className="truncate">
            <Trigger visibility={visibility} people={people} replyTo={replyTo} />
          </span>
        </DropdownMenuTrigger>

        <DropdownMenuContent align="start" className="w-72">
          {isMessage ? (
            <>
              <DropdownMenuRadioGroup value="direct">
                <DropdownMenuLabel>{t({ id: 'compose.visibility.title', defaultMessage: 'Visibility' })}</DropdownMenuLabel>
                <DropdownMenuRadioItem value="direct" disabled className={leadingCheck}>
                  {t({ id: 'eunha.compose_audience.everyone_mentioned', defaultMessage: 'Everyone mentioned' })}
                </DropdownMenuRadioItem>
              </DropdownMenuRadioGroup>
              <DropdownMenuSeparator />
              <DropdownMenuItem onClick={() => setConfirmingPost(true)}>
                <Newspaper /> {t({ id: 'eunha.compose_audience.compose_a_post_instead', defaultMessage: 'Compose a post instead' })}
              </DropdownMenuItem>
            </>
          ) : (
            <>
              <DropdownMenuRadioGroup
                value={isFollowers ? 'private' : 'public'}
                onValueChange={(v) =>
                  onVisibilityChange(v === 'private' ? 'private' : 'public')
                }
              >
                <DropdownMenuLabel>{t({ id: 'compose.visibility.title', defaultMessage: 'Visibility' })}</DropdownMenuLabel>
                <DropdownMenuRadioItem value="public" className={leadingCheck}>{t({ id: 'privacy.public.short', defaultMessage: 'Public' })}</DropdownMenuRadioItem>
                <DropdownMenuRadioItem value="private" className={leadingCheck}>{t({ id: 'account.followers', defaultMessage: 'Followers' })}</DropdownMenuRadioItem>
              </DropdownMenuRadioGroup>

              <DropdownMenuSeparator />

              {/* Discoverability is fixed off for followers-only posts. */}
              <SwitchItem
                icon={<Search />}
                label={t({ id: 'eunha.compose_audience.discoverable_in_public_feeds_search_results', defaultMessage: 'Discoverable in public feeds & search results' })}
                checked={visibility === 'public'}
                disabled={isFollowers}
                onChange={setDiscoverable}
              />
              <DropdownMenuSeparator />
              <DropdownMenuItem onClick={() => onVisibilityChange('direct')}>
                <MessageCircle /> {t({ id: 'eunha.compose_audience.compose_a_message_instead', defaultMessage: 'Compose a message instead' })}
              </DropdownMenuItem>
            </>
          )}
        </DropdownMenuContent>
      </DropdownMenu>

      {/* Message → post is the direction that can publish something written in
          private, so it asks. Post → message does not need to. */}
      <AlertDialog open={confirmingPost} onOpenChange={setConfirmingPost}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t({ id: 'compose.switch_modal.title', defaultMessage: 'Convert to post?' })}</AlertDialogTitle>
            <AlertDialogDescription>
              {t({ id: 'eunha.compose_audience.your_message_has_limited_visibility_if', defaultMessage: 'Your message has limited visibility. If you convert it to a post, it will switch to your default post visibility.' })}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>{t({ id: 'column_back_button.label', defaultMessage: 'Back' })}</AlertDialogCancel>
            <AlertDialogAction
              onClick={() => {
                // Upstream switches to the account's default here rather than
                // to public, which the dialog's own wording promises.
                onVisibilityChange(
                  defaultVisibility === 'direct' ? 'public' : defaultVisibility,
                )
                setConfirmingPost(false)
              }}
            >
              {t({ id: 'compose.switch_modal.continue', defaultMessage: 'Continue' })}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  )
}
