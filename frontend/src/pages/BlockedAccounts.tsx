import { useState } from 'react'
import { NavLink, useLocation } from 'react-router-dom'
import { toast } from 'sonner'

import type { mastodon } from '../masto.ts'
import { getBlocks, getMutes, setBlock, setMute } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { AccountRow } from '@/components/account-row.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import { cn, errorMessage } from '@/lib/utils.ts'
import { AccountListSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const tab =
  'border-b-2 border-transparent px-3 py-2 text-sm font-medium text-muted-foreground no-underline hover:text-foreground'
const cls = ({ isActive }: { isActive: boolean }) =>
  cn(tab, isActive && 'border-primary text-foreground')

export default function BlockedAccounts() {
  const muted = useLocation().pathname.endsWith('/muted')
  const token = getToken()
  const [busy, setBusy] = useState<string | null>(null)

  const feed = useInfinitePaginator<mastodon.v1.Account>(
    () => (muted ? getMutes(token ?? '') : getBlocks(token ?? '')),
    [muted, token],
  )
  const accounts = feed.items

  // Undoing removes the row rather than reloading the page: the list is the
  // record of a decision, and a row that stays put reads as a failed click.
  const undo = async (account: mastodon.v1.Account) => {
    if (!token || busy) return
    setBusy(account.id)
    try {
      if (muted) await setMute(account.id, token, false)
      else await setBlock(account.id, token, false)
      feed.mutate((items) => items.filter((a) => a.id !== account.id))
      toast.success(`${muted ? t({ id: 'eunha.blocked_accounts.unmuted', defaultMessage: 'Unmuted' }) : t({ id: 'eunha.blocked_accounts.unblocked', defaultMessage: 'Unblocked' })} @${account.acct}.`)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(null)
    }
  }

  if (!token) {
    return (
      <div className="page-frame">
        <TopBar />
        <Card>
          <CardContent className="space-y-3 py-6 text-center">
            <p className="text-muted-foreground text-sm">
              {t({ id: 'eunha.blocked_accounts.sign_in_to_see_the_accounts', defaultMessage: 'Sign in to see the accounts you have blocked or muted.' })}
            </p>
            <Button onClick={() => void beginLogin()}>{t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}</Button>
          </CardContent>
        </Card>
      </div>
    )
  }

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-2 text-lg font-bold">{t({ id: 'eunha.blocked_accounts.blocked_and_muted', defaultMessage: 'Blocked and muted' })}</h1>
      <nav className="mb-2 flex gap-1 border-b">
        <NavLink to="/blocked" className={cls}>
          {t({ id: 'account.blocked', defaultMessage: 'Blocked' })}
        </NavLink>
        <NavLink to="/domain_blocks" className={cls}>{t({ id: 'eunha.blocked_accounts.domains', defaultMessage: 'Domains' })}</NavLink>
        <NavLink to="/muted" className={cls}>
          {t({ id: 'account.muted', defaultMessage: 'Muted' })}
        </NavLink>
      </nav>
      {feed.error && <p className="text-destructive text-sm">{feed.error}</p>}
      <div className="space-y-1">
        {accounts === null && !feed.error && (
          <AccountListSkeleton />
        )}
        {accounts?.map((a) => (
          <AccountRow
            key={a.id}
            account={a}
            action={
              <Button
                size="sm"
                variant="outline"
                disabled={busy === a.id}
                onClick={() => void undo(a)}
              >
                {muted ? t({ id: 'account.unmute_short', defaultMessage: 'Unmute' }) : t({ id: 'account.unblock_short', defaultMessage: 'Unblock' })}
              </Button>
            }
          />
        ))}
        {accounts?.length === 0 && (
          <p className="text-muted-foreground text-sm">
            {muted ? t({ id: 'eunha.blocked_accounts.you_have_not_muted_anyone', defaultMessage: 'You have not muted anyone.' }) : t({ id: 'eunha.blocked_accounts.you_have_not_blocked_anyone', defaultMessage: 'You have not blocked anyone.' })}
          </p>
        )}
        <InfiniteScroll
          onLoadMore={feed.loadMore}
          loading={feed.loadingMore}
          done={feed.done}
          hasItems={!!accounts?.length}
        />
      </div>
    </div>
  )
}
