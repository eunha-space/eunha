import { useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import {
  createUsernameBlock,
  deleteUsernameBlock,
  listUsernameBlocks,
  updateUsernameBlock,
  type UsernameBlock,
  type UsernameComparison,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ChoiceSelect, ConfirmButton } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { BLOCK_TABS } from './IpBlocks.tsx'

// Mastodon's `simple_form` labels for `UsernameBlock#comparison`.
const COMPARISONS: Record<UsernameComparison, string> = {
  equals: 'Is equal to',
  contains: 'Contains',
}

/**
 * Usernames sign-ups may not take: Mastodon 4.7's `Admin::UsernameBlocksController`.
 * A rule matches a username equal to it or containing it, once both are
 * lowercased and digits read as the letters they pass for.
 */
export default function UsernameBlocks() {
  const token = getToken()
  const [blocks, setBlocks] = useState<UsernameBlock[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [username, setUsername] = useState('')
  const [comparison, setComparison] = useState<UsernameComparison>('equals')
  const [withApproval, setWithApproval] = useState(false)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    if (!token) return
    listUsernameBlocks(token)
      .then((b) => {
        setBlocks(b)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }, [token])

  const add = async (e: FormEvent) => {
    e.preventDefault()
    if (!token || !username.trim()) return
    setSaving(true)
    try {
      const block = await createUsernameBlock(token, {
        username: username.trim(),
        comparison,
        allow_with_approval: withApproval,
      })
      setBlocks((list) =>
        [...(list ?? []), block].sort((a, b) => a.username.localeCompare(b.username)),
      )
      setUsername('')
      setWithApproval(false)
      toast.success('Username rule added.')
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  const toggle = async (block: UsernameBlock, patch: Partial<UsernameBlock>) => {
    try {
      const updated = await updateUsernameBlock(token ?? '', block.id, patch)
      setBlocks((list) => (list ?? []).map((b) => (b.id === block.id ? updated : b)))
      toast.success('Username rule updated.')
    } catch (err) {
      toast.error(errorMessage(err))
    }
  }

  return (
    <AdminLayout title="Usernames" permission="manage_blocks" sub={BLOCK_TABS}>
      <form onSubmit={add} className="mb-4 space-y-2 rounded-lg border p-3">
        <div className="flex flex-wrap gap-2">
          <ChoiceSelect
            label="Comparison"
            value={comparison}
            items={COMPARISONS}
            onChange={setComparison}
            className="w-40"
          />
          <Input
            aria-label="Username"
            placeholder="username"
            className="min-w-0 flex-1"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
          />
          <Button type="submit" disabled={saving || !username.trim()}>
            Add rule
          </Button>
        </div>
        <Label className="text-sm font-normal">
          <Checkbox
            checked={withApproval}
            onCheckedChange={(v) => setWithApproval(v === true)}
          />
          Allow registrations with approval
        </Label>
      </form>
      <AdminError error={error} />
      {blocks === null && !error && <p className="text-muted-foreground text-sm">Loading…</p>}
      <div className={blocks?.length ? 'divide-y rounded-lg border' : ''}>
        {blocks?.map((b) => (
          <div key={b.id} className="flex flex-wrap items-center gap-2 p-2.5">
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <span className="truncate font-mono text-sm">{b.username}</span>
                <Badge variant="outline">{COMPARISONS[b.comparison]}</Badge>
                <Badge variant={b.allow_with_approval ? 'secondary' : 'destructive'}>
                  {b.allow_with_approval ? 'Needs approval' : 'Blocked'}
                </Badge>
              </div>
            </div>
            <Button
              size="xs"
              variant="outline"
              onClick={() =>
                void toggle(b, {
                  comparison: b.comparison === 'equals' ? 'contains' : 'equals',
                })
              }
            >
              {b.comparison === 'equals' ? 'Match containing' : 'Match exactly'}
            </Button>
            <Button
              size="xs"
              variant="outline"
              onClick={() => void toggle(b, { allow_with_approval: !b.allow_with_approval })}
            >
              {b.allow_with_approval ? 'Block outright' : 'Allow with approval'}
            </Button>
            <ConfirmButton
              size="xs"
              title={`Remove the rule for ${b.username}?`}
              description="Sign-ups with matching usernames are no longer refused."
              confirmLabel="Remove"
              onConfirm={async () => {
                await deleteUsernameBlock(token ?? '', b.id)
                setBlocks((list) => (list ?? []).filter((x) => x.id !== b.id))
                toast.success('Username rule removed.')
              }}
            >
              Remove
            </ConfirmButton>
          </div>
        ))}
      </div>
      {blocks?.length === 0 && (
        <p className="text-muted-foreground text-sm">No usernames are blocked.</p>
      )}
    </AdminLayout>
  )
}
