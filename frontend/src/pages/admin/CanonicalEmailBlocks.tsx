import { useState, type FormEvent } from 'react'
import { toast } from 'sonner'

import {
  createCanonicalEmailBlock,
  deleteCanonicalEmailBlock,
  listCanonicalEmailBlocks,
  testCanonicalEmailBlock,
  type CanonicalEmailBlock,
} from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ChoiceSelect, ConfirmButton } from '@/components/admin/admin-common.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { BLOCK_TABS } from './IpBlocks.tsx'

const MODES = { email: 'By email address', hash: 'By canonical hash' }

/**
 * Single addresses kept from signing up again — Mastodon blocks these
 * automatically when it suspends a local account, and stores only a hash of
 * the address in its canonical form (lower case, no dots or `+tag` in the
 * local part), so the list shows hashes and the test box is how to check one.
 */
export default function CanonicalEmailBlocks() {
  const token = getToken()
  const feed = useInfinitePaginator<CanonicalEmailBlock>(
    () => listCanonicalEmailBlocks(token ?? ''),
    [token],
  )
  const [testEmail, setTestEmail] = useState('')
  const [testResult, setTestResult] = useState<string | null>(null)
  const [mode, setMode] = useState<keyof typeof MODES>('email')
  const [value, setValue] = useState('')
  const [saving, setSaving] = useState(false)

  const test = async (e: FormEvent) => {
    e.preventDefault()
    if (!token || !testEmail.trim()) return
    try {
      const matches = await testCanonicalEmailBlock(token, testEmail.trim())
      setTestResult(
        matches.length > 0
          ? `Blocked: matches ${matches.map((m) => m.canonical_email_hash.slice(0, 12)).join(', ')}…`
          : 'Not blocked.',
      )
    } catch (err) {
      toast.error(errorMessage(err))
    }
  }

  const add = async (e: FormEvent) => {
    e.preventDefault()
    if (!token || !value.trim()) return
    setSaving(true)
    try {
      const block = await createCanonicalEmailBlock(
        token,
        mode === 'email' ? { email: value.trim() } : { canonical_email_hash: value.trim() },
      )
      feed.mutate((items) => [block, ...items])
      setValue('')
      toast.success('Address blocked.')
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <AdminLayout title="Email addresses" permission="manage_blocks" sub={BLOCK_TABS}>
      <div className="mb-4 grid gap-3 sm:grid-cols-2">
        <form onSubmit={test} className="space-y-2 rounded-lg border p-3">
          <h2 className="text-sm font-semibold">Test an address</h2>
          <div className="flex gap-2">
            <Input
              type="email"
              aria-label="Address to test"
              placeholder="someone@example.com"
              value={testEmail}
              onChange={(e) => {
                setTestEmail(e.target.value)
                setTestResult(null)
              }}
            />
            <Button type="submit" variant="outline" disabled={!testEmail.trim()}>
              Test
            </Button>
          </div>
          {testResult && <p className="text-sm">{testResult}</p>}
        </form>
        <form onSubmit={add} className="space-y-2 rounded-lg border p-3">
          <h2 className="text-sm font-semibold">Block an address</h2>
          <ChoiceSelect label="Block by" value={mode} items={MODES} onChange={setMode} />
          <div className="flex gap-2">
            <Input
              aria-label={mode === 'email' ? 'Address to block' : 'Hash to block'}
              placeholder={mode === 'email' ? 'someone@example.com' : 'SHA-256 hex digest'}
              value={value}
              onChange={(e) => setValue(e.target.value)}
            />
            <Button type="submit" disabled={saving || !value.trim()}>
              Block
            </Button>
          </div>
        </form>
      </div>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      <div className={feed.items?.length ? 'divide-y rounded-lg border' : ''}>
        {feed.items?.map((b) => (
          <div key={b.id} className="flex items-center gap-2 p-2.5">
            <span className="min-w-0 flex-1 truncate font-mono text-xs">
              {b.canonical_email_hash}
            </span>
            <ConfirmButton
              size="xs"
              title="Unblock this address?"
              description="Whoever owns it can sign up again."
              confirmLabel="Unblock"
              onConfirm={async () => {
                await deleteCanonicalEmailBlock(token ?? '', b.id)
                feed.mutate((items) => items.filter((x) => x.id !== b.id))
                toast.success('Address unblocked.')
              }}
            >
              Unblock
            </ConfirmButton>
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">No addresses are blocked.</p>
      )}
      <InfiniteScroll
        onLoadMore={feed.loadMore}
        loading={feed.loadingMore}
        done={feed.done}
        hasItems={!!feed.items?.length}
      />
    </AdminLayout>
  )
}
