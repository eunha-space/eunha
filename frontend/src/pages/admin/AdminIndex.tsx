import { Link } from 'react-router-dom'
import { ChevronRight } from 'lucide-react'

import { AdminLayout, useAdminSections } from '@/components/admin/admin-layout.tsx'
import { ADMIN_SECTIONS, SERVER_GROUP_START, type AdminSection } from '@/lib/admin-sections.ts'
import { t } from '@/i18n.tsx'

const row = 'hover:bg-muted/60 flex items-center gap-3 px-4 py-3 text-inherit no-underline'

function Row({ to, label, nested }: { to: string; label: string; nested?: boolean }) {
  return (
    <li>
      <Link to={to} className={nested ? `${row} pl-8` : row}>
        <span className="min-w-0 flex-1">{label}</span>
        <ChevronRight className="text-muted-foreground size-4 shrink-0" aria-hidden />
      </Link>
    </li>
  )
}

/** A group of sections; one with several pages lists them under its name. */
function Group({ title, sections }: { title: string; sections: AdminSection[] }) {
  if (sections.length === 0) return null
  return (
    <section className="space-y-2">
      <h2 className="text-muted-foreground text-sm font-medium">{title}</h2>
      <ul className="divide-y rounded-lg border">
        {sections.map((s) =>
          s.pages ? (
            <li key={s.to}>
              <p className="px-4 pt-3 pb-1 font-medium">{s.label}</p>
              <ul>
                {s.pages.map((p) => (
                  <Row key={p.to} to={p.to} label={p.label} nested />
                ))}
              </ul>
            </li>
          ) : (
            <Row key={s.to} to={s.to} label={s.label} />
          ),
        )}
      </ul>
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
        <Group title={t({ id: 'navigation_bar.moderation', defaultMessage: 'Moderation' })} sections={sections.filter((s) => !isServer(s))} />
        <Group title={t({ id: 'navigation_bar.administration', defaultMessage: 'Administration' })} sections={sections.filter(isServer)} />
      </nav>
    </AdminLayout>
  )
}
