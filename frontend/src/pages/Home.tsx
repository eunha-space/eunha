import { getToken } from '../auth.ts'
import { isAdvancedLayout } from '../lib/panes.ts'
import { useIsMobile } from '@/hooks/use-mobile.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { StatusFeed } from '@/components/status-feed.tsx'
import { ColumnHeader } from '@/components/column-header.tsx'
import { useComposeModal } from '@/components/compose-modal.tsx'
import { AdvancedLayout } from '@/components/advanced-layout.tsx'
import {
  AnnouncementsButton,
  AnnouncementsPanel,
  useAnnouncements,
} from '@/components/announcements.tsx'
import PublicTimeline from './PublicTimeline.tsx'

// Signed out there is no home timeline, and what a visitor arrived to look at
// is the instance's own posts — so "/" is the local timeline rather than a
// paragraph about the software running it. What the instance says about itself
// still has a page of its own at /about.
//
// A dispatcher rather than a branch inside the timeline: signing in or out
// reloads the page, so the two never swap under a mounted component, and each
// keeps its own hooks. The advanced layout joins it on the same terms.
//
// A narrow screen overrides the preference rather than consulting it. The
// advanced layout is a row of 24rem panes that scrolls sideways, which on a
// phone is one column you cannot see the edges of and a second you have to
// swipe the first away to reach — so it is not a worse way to read a phone,
// it is not a way to read a phone. The setting is per browser, so a phone can
// already be left on the single column; this covers the two cases that are
// not a choice anyone made — a desktop browser dragged narrow, and the same
// browser profile syncing its localStorage to a phone.
export default function Home() {
  const isMobile = useIsMobile()
  if (!getToken()) return <PublicTimeline local />
  return isAdvancedLayout() && !isMobile ? <AdvancedLayout /> : <HomeTimeline />
}

function HomeTimeline() {
  const token = getToken()
  const { openCompose } = useComposeModal()
  const announcements = useAnnouncements(token)

  return (
    <>
      <TopBar />
      <div className="column-frame">
        {/* "Following", not "Home": the rail already says where you are, and
            this names what the feed actually is. Upstream keeps "Home" as the
            column's accessible label for the same reason. */}
        <ColumnHeader title="Following">
          {announcements.items.length > 0 && (
            <AnnouncementsButton
              unread={announcements.unread}
              shown={announcements.shown}
              onToggle={announcements.toggle}
            />
          )}
        </ColumnHeader>
        {announcements.shown && token && (
          <AnnouncementsPanel
            items={announcements.items}
            token={token}
            update={announcements.update}
          />
        )}
        <section aria-label="Home" className="space-y-2 p-3">
          <StatusFeed
            kind="home"
            token={token}
            onReply={(status, prepend) =>
              openCompose({ replyTo: status, onPosted: prepend })
            }
          />
        </section>
      </div>
    </>
  )
}
