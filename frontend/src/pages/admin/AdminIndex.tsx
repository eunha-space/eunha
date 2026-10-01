import { Navigate } from 'react-router-dom'

import { AdminLayout, useRolePermissions } from '@/components/admin/admin-layout.tsx'
import { firstAdminSection } from '@/lib/admin-sections.ts'
import { getToken } from '../../auth.ts'

/**
 * `/admin`: the first section this role may open — the dashboard for an
 * admin, the report queue for a moderator who cannot see the dashboard.
 */
export default function AdminIndex() {
  const permissions = useRolePermissions()
  const section = permissions === null ? null : firstAdminSection(permissions)
  if (section && getToken()) return <Navigate to={section.to} replace />
  // Signed out, still loading, or no moderation permission at all: the layout
  // says which.
  return (
    <AdminLayout title="Moderation" permission="manage_reports">
      {null}
    </AdminLayout>
  )
}
