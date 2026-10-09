import type { ReactNode } from 'react'
import { Link } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { useAnimatedImage } from '@/hooks/use-animated-image.ts'
import { DisplayName } from '@/components/emoji.tsx'

export function AccountRow({
  account,
  action,
}: {
  account: mastodon.v1.Account
  // Optional trailing controls (e.g. accept/reject) rendered outside the link
  // so clicking them doesn't navigate to the profile.
  action?: ReactNode
}) {
  const name = account.displayName || account.username
  const avatar = useAnimatedImage(account.avatar, account.avatarStatic)
  return (
    <div className="flex items-center gap-2">
      <Link
        to={`/@${account.acct}`}
        className="hover:bg-muted/50 flex min-w-0 flex-1 items-center gap-3 rounded-lg p-2 no-underline"
      >
        <Avatar className="size-10" {...avatar.hover}>
          <AvatarImage src={avatar.src} alt="" />
          <AvatarFallback>{name.slice(0, 1).toUpperCase()}</AvatarFallback>
        </Avatar>
        <div className="min-w-0">
          <DisplayName account={account} className="block truncate font-medium" />
          <div className="text-muted-foreground truncate text-sm">@{account.acct}</div>
        </div>
      </Link>
      {action && <div className="shrink-0 pr-2">{action}</div>}
    </div>
  )
}
