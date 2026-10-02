import { useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import { can } from '../../admin-api.ts'
import {
  createAnnouncement,
  deleteAnnouncement,
  distributeAnnouncement,
  listAnnouncements,
  previewAnnouncement,
  setAnnouncementPublished,
  testAnnouncement,
  updateAnnouncement,
  type AdminAnnouncement,
  type AnnouncementParams,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout, useRolePermissions } from '@/components/admin/admin-layout.tsx'
import { ChoiceSelect, ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'

/** An ISO time as a `datetime-local` value, in the browser's zone. */
function toLocalInput(value: string | null): string {
  if (!value) return ''
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return ''
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}`
}

/** A `datetime-local` value as an ISO time, or blank. */
function fromLocalInput(value: string): string {
  if (!value) return ''
  const date = new Date(value)
  return Number.isNaN(date.getTime()) ? '' : date.toISOString()
}

function AnnouncementForm({
  initial,
  onSaved,
  onCancel,
}: {
  initial: AdminAnnouncement | null
  onSaved: () => void
  onCancel?: () => void
}) {
  const token = getToken()
  const [text, setText] = useState(initial?.text ?? '')
  const [startsAt, setStartsAt] = useState(toLocalInput(initial?.starts_at ?? null))
  const [endsAt, setEndsAt] = useState(toLocalInput(initial?.ends_at ?? null))
  const [allDay, setAllDay] = useState(initial?.all_day ?? false)
  const [scheduledAt, setScheduledAt] = useState(toLocalInput(initial?.scheduled_at ?? null))
  const [saving, setSaving] = useState(false)

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setSaving(true)
    const params: AnnouncementParams = {
      text,
      starts_at: fromLocalInput(startsAt),
      ends_at: fromLocalInput(endsAt),
      all_day: allDay,
      scheduled_at: fromLocalInput(scheduledAt),
    }
    try {
      if (initial) await updateAnnouncement(token ?? '', initial.id, params)
      else {
        const created = await createAnnouncement(token ?? '', params)
        toast.success(created.published ? 'Announcement published!' : 'Announcement scheduled!')
        setText('')
        setStartsAt('')
        setEndsAt('')
        setScheduledAt('')
        setAllDay(false)
      }
      if (initial) toast.success('Announcement updated!')
      onSaved()
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  const id = initial?.id ?? 'new'
  return (
    <form onSubmit={submit} className="space-y-3 rounded-lg border p-3">
      <div className="space-y-1">
        <Label htmlFor={`announcement-text-${id}`}>Text</Label>
        <Textarea
          id={`announcement-text-${id}`}
          rows={3}
          value={text}
          onChange={(e) => setText(e.target.value)}
        />
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <div className="space-y-1">
          <Label htmlFor={`announcement-starts-${id}`}>Event start</Label>
          <Input
            id={`announcement-starts-${id}`}
            type="datetime-local"
            value={startsAt}
            onChange={(e) => setStartsAt(e.target.value)}
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor={`announcement-ends-${id}`}>Event end</Label>
          <Input
            id={`announcement-ends-${id}`}
            type="datetime-local"
            value={endsAt}
            onChange={(e) => setEndsAt(e.target.value)}
          />
        </div>
      </div>
      <p className="text-muted-foreground text-xs">
        If the announcement is bound to a time range, give both its start and end. The
        announcement is unpublished once it ends.
      </p>
      <Label className="text-sm font-normal">
        <Switch checked={allDay} onCheckedChange={setAllDay} />
        All-day event
      </Label>
      {!initial?.published && (
        <div className="space-y-1">
          <Label htmlFor={`announcement-scheduled-${id}`}>Schedule publication</Label>
          <Input
            id={`announcement-scheduled-${id}`}
            type="datetime-local"
            value={scheduledAt}
            onChange={(e) => setScheduledAt(e.target.value)}
          />
          <p className="text-muted-foreground text-xs">
            Leave blank to publish the announcement immediately.
          </p>
        </div>
      )}
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={saving || !text.trim()}>
          {initial ? 'Save changes' : 'Create announcement'}
        </Button>
        {onCancel && (
          <Button type="button" size="sm" variant="outline" onClick={onCancel}>
            Cancel
          </Button>
        )}
      </div>
    </form>
  )
}

const FILTERS = { all: 'All', published: 'Published', unpublished: 'Unpublished' }

/**
 * Announcements: Mastodon's `Admin::AnnouncementsController`, and mailing an
 * announcement to every user, which takes `manage_settings` too.
 */
export default function Announcements() {
  const token = getToken()
  const permissions = useRolePermissions()
  const [filter, setFilter] = useState<keyof typeof FILTERS>('all')
  const [items, setItems] = useState<AdminAnnouncement[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [editing, setEditing] = useState<string | null>(null)
  const mayDistribute = permissions !== null && can(permissions, 'manage_settings')

  const load = () => {
    if (!token) return
    listAnnouncements(token, filter === 'all' ? undefined : filter)
      .then((list) => {
        setItems(list)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token, filter])

  const act = async (run: () => Promise<unknown>, done: string) => {
    try {
      await run()
      toast.success(done)
      load()
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <AdminLayout
      title="Announcements"
      permission="manage_announcements"
      actions={
        <ChoiceSelect
          label="Show"
          value={filter}
          items={FILTERS}
          onChange={setFilter}
          className="w-40"
        />
      }
    >
      <div className="mb-4">
        <AnnouncementForm initial={null} onSaved={load} />
      </div>
      <AdminError error={error} />
      {items === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      {items?.length === 0 && (
        <p className="text-muted-foreground text-sm">No announcements found.</p>
      )}
      <div className="space-y-2">
        {items?.map((a) =>
          editing === a.id ? (
            <AnnouncementForm
              key={a.id}
              initial={a}
              onCancel={() => setEditing(null)}
              onSaved={() => {
                setEditing(null)
                load()
              }}
            />
          ) : (
            <div key={a.id} className="space-y-2 rounded-lg border p-3">
              <div className="flex flex-wrap items-center gap-2 text-xs">
                {a.published ? (
                  <Badge>Published</Badge>
                ) : a.scheduled_at ? (
                  <Badge variant="outline">Scheduled for {formatDate(a.scheduled_at)}</Badge>
                ) : (
                  <Badge variant="outline">Unpublished</Badge>
                )}
                {a.starts_at && (
                  <span className="text-muted-foreground">
                    {formatDate(a.starts_at)} – {formatDate(a.ends_at)}
                    {a.all_day && ' (all day)'}
                  </span>
                )}
                {a.notification_sent_at && (
                  <span className="text-muted-foreground">
                    Mailed {formatDate(a.notification_sent_at)}
                  </span>
                )}
              </div>
              <div
                className="text-sm break-words [&_a]:text-primary [&_a]:underline"
                dangerouslySetInnerHTML={{ __html: a.content }}
              />
              <div className="flex flex-wrap gap-2">
                <Button size="xs" variant="outline" onClick={() => setEditing(a.id)}>
                  Edit
                </Button>
                <Button
                  size="xs"
                  variant="outline"
                  onClick={() =>
                    void act(
                      () => setAnnouncementPublished(token ?? '', a.id, !a.published),
                      a.published ? 'Announcement unpublished.' : 'Announcement published!',
                    )
                  }
                >
                  {a.published ? 'Unpublish' : 'Publish'}
                </Button>
                {mayDistribute && a.published && !a.notification_sent_at && (
                  <>
                    <Button
                      size="xs"
                      variant="outline"
                      onClick={() =>
                        void act(
                          () => testAnnouncement(token ?? '', a.id),
                          'A test email has been sent to you.',
                        )
                      }
                    >
                      Send test email
                    </Button>
                    <ConfirmButton
                      size="xs"
                      destructive={false}
                      title="Notify users by email?"
                      description="Every confirmed user is mailed this announcement, once."
                      confirmLabel="Send"
                      onConfirm={async () => {
                        const preview = await previewAnnouncement(token ?? '', a.id)
                        await distributeAnnouncement(token ?? '', a.id)
                        toast.success(`Sending to ${preview.user_count} users.`)
                        load()
                      }}
                    >
                      Notify users via email
                    </ConfirmButton>
                  </>
                )}
                <ConfirmButton
                  size="xs"
                  title="Delete this announcement?"
                  description="It disappears for everyone, with its reactions."
                  confirmLabel="Delete"
                  onConfirm={async () => {
                    await deleteAnnouncement(token ?? '', a.id)
                    toast.success('Announcement deleted.')
                    load()
                  }}
                >
                  Delete
                </ConfirmButton>
              </div>
            </div>
          ),
        )}
      </div>
    </AdminLayout>
  )
}
