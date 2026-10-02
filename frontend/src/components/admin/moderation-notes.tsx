import { useCallback, useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import { type ModerationNote } from '../../admin-api.ts'
import { getMeId } from '../../me.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminAccountLink, ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'

/** `ReportNote::CONTENT_SIZE_LIMIT` and `AccountModerationNote::CONTENT_SIZE_LIMIT`. */
const CONTENT_SIZE_LIMIT = 2000

/** An extra submit button beside "Add note", such as add-and-resolve. */
export interface NoteSubmit {
  label: string
  /** What is sent with the note. */
  flags: Record<string, boolean>
}

/**
 * The notes moderators leave each other, oldest first, with the form to add
 * one: Mastodon's report notes and account moderation notes. The server says
 * who may delete which; the button is offered on every note and a refusal
 * comes back as a toast.
 */
export function ModerationNotes({
  load,
  create,
  remove,
  extraSubmits = [],
  onCreated,
}: {
  load: () => Promise<ModerationNote[]>
  create: (content: string, flags: Record<string, boolean>) => Promise<ModerationNote>
  remove: (id: string) => Promise<void>
  extraSubmits?: NoteSubmit[]
  /** After a note is added, with the flags it went with. */
  onCreated?: (flags: Record<string, boolean>) => void
}) {
  const [notes, setNotes] = useState<ModerationNote[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [content, setContent] = useState('')
  const [saving, setSaving] = useState(false)
  const meId = getMeId()

  const refresh = useCallback(() => {
    load()
      .then((n) => {
        setNotes(n)
        setError(null)
      })
      .catch((e) => setError(errorMessage(e)))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => {
    refresh()
  }, [refresh])

  const submit = async (flags: Record<string, boolean>, e?: FormEvent) => {
    e?.preventDefault()
    if (saving || !content.trim()) return
    setSaving(true)
    try {
      const note = await create(content, flags)
      setNotes((n) => [...(n ?? []), note])
      setContent('')
      toast.success('Note added.')
      onCreated?.(flags)
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="space-y-2">
      <h2 className="text-sm font-semibold">Moderation notes</h2>
      {error && <p className="text-destructive text-sm">{error}</p>}
      {notes?.length === 0 && (
        <p className="text-muted-foreground text-sm">No notes yet.</p>
      )}
      {notes?.map((note) => (
        <article key={note.id} className="space-y-1 rounded-lg border p-3">
          <div className="flex items-center gap-2">
            <div className="min-w-0 flex-1">
              {note.account && <AdminAccountLink account={note.account} size="sm" />}
            </div>
            <span className="text-muted-foreground shrink-0 text-xs">
              {formatDate(note.created_at)}
            </span>
            <ConfirmButton
              size="xs"
              variant="ghost"
              title="Delete this note?"
              description="The note is removed for every moderator."
              confirmLabel="Delete"
              onConfirm={async () => {
                await remove(note.id)
                setNotes((n) => (n ?? []).filter((x) => x.id !== note.id))
                toast.success('Note deleted.')
              }}
            >
              {note.account?.id === meId ? 'Delete' : 'Delete…'}
            </ConfirmButton>
          </div>
          <p className="text-sm whitespace-pre-wrap">{note.content}</p>
        </article>
      ))}
      <form onSubmit={(e) => void submit({}, e)} className="space-y-2">
        <Textarea
          aria-label="New note"
          placeholder="Only other moderators see notes."
          value={content}
          maxLength={CONTENT_SIZE_LIMIT}
          rows={3}
          className="resize-y"
          onChange={(e) => setContent(e.target.value)}
        />
        <div className="flex flex-wrap gap-2">
          <Button type="submit" size="sm" disabled={saving || !content.trim()}>
            Add note
          </Button>
          {extraSubmits.map((s) => (
            <Button
              key={s.label}
              type="button"
              size="sm"
              variant="outline"
              disabled={saving || !content.trim()}
              onClick={() => void submit(s.flags)}
            >
              {s.label}
            </Button>
          ))}
        </div>
      </form>
    </section>
  )
}
