import { Link } from 'react-router-dom'
import { ChevronRight } from 'lucide-react'

import { AdminLayout, useAdminSections } from '@/components/admin/admin-layout.tsx'
import { Button } from '@/components/ui/button.tsx'
import {
  Item, ItemActions, ItemContent, ItemFooter, ItemGroup, ItemMedia, ItemTitle,
} from '@/components/ui/item.tsx'
import { ADMIN_SECTIONS, SERVER_GROUP_START, type AdminSection } from '@/lib/admin-sections.ts'
import { t } from '@/i18n.tsx'

const media = 'bg-muted size-9 rounded-md'

/**
 * One section: a link to it, or, for a section of several pages, its name
 * with a button for each page.
 */
function Section({ section: { to, icon: Icon, label, pages } }: { section: AdminSection }) {
  if (pages) {
    return (
      <Item variant="outline">
        <ItemMedia variant="icon" className={media}>
          <Icon className="size-4" />
        </ItemMedia>
        <ItemContent>
          <ItemTitle>{label}</ItemTitle>
        </ItemContent>
        <ItemFooter className="flex-wrap justify-start pl-12">
          {pages.map((page) => (
            <Button key={page.to} variant="outline" size="sm" render={<Link to={page.to} />}>
              {page.label}
            </Button>
          ))}
        </ItemFooter>
      </Item>
    )
  }
  return (
    <Item variant="outline" render={<Link to={to} />} className="text-inherit no-underline">
      <ItemMedia variant="icon" className={media}>
        <Icon className="size-4" />
      </ItemMedia>
      <ItemContent>
        <ItemTitle>{label}</ItemTitle>
      </ItemContent>
      <ItemActions>
        <ChevronRight className="text-muted-foreground size-4" aria-hidden />
      </ItemActions>
    </Item>
  )
}

function Group({ title, sections }: { title?: string; sections: AdminSection[] }) {
  if (sections.length === 0) return null
  return (
    <section className="space-y-2">
      {title && <h2 className="text-muted-foreground text-sm font-medium">{title}</h2>}
      <ItemGroup className="gap-2">
        {sections.map((s) => (
          <div key={s.to} role="listitem">
            <Section section={s} />
          </div>
        ))}
      </ItemGroup>
    </section>
  )
}

/**
 * `/admin`: every section this role may open, moderation first and then the
 * server's administration, each opening its own page.
 */
export default function AdminIndex() {
  const sections = useAdminSections() ?? []
  const start = ADMIN_SECTIONS.findIndex((s) => s.to === SERVER_GROUP_START)
  const isServer = (s: AdminSection) => ADMIN_SECTIONS.indexOf(s) >= start
  // Gated on whichever section the role may open first, as any one is
  // reason enough to be here.
  const permission = sections[0]?.permission ?? 'manage_reports'

  return (
    <AdminLayout title={t({ id: 'navigation_bar.moderation', defaultMessage: 'Moderation' })} permission={permission} back={false}>
      <nav aria-label={t({ id: 'eunha.admin.admin_layout.moderation_sections', defaultMessage: 'Moderation sections' })} className="space-y-6">
        {/* The page's own title already says Moderation. */}
        <Group sections={sections.filter((s) => !isServer(s))} />
        <Group title={t({ id: 'navigation_bar.administration', defaultMessage: 'Administration' })} sections={sections.filter(isServer)} />
      </nav>
    </AdminLayout>
  )
}
