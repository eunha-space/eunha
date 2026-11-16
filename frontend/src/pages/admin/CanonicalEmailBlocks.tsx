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
import { RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const MODES = { get email() { return t({ id: 'eunha.admin.canonical_email_blocks.by_email_address', defaultMessage: 'By email address' }) }, get hash() { return t({ id: 'eunha.admin.canonical_email_blocks.by_canonical_hash', defaultMessage: 'By canonical hash' }) } }

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
          ? t({ id: 'eunha.admin.canonical_email_blocks.blocked_matches', defaultMessage: 'Blocked: matches {hashes}…' }, { hashes: matches.map((m) => m.canonical_email_hash.slice(0, 12)).join(', ') })
          : t({ id: 'eunha.admin.canonical_email_blocks.not_blocked', defaultMessage: 'Not blocked.' }),
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
      toast.success(t({ id: 'eunha.admin.canonical_email_blocks.address_blocked', defaultMessage: 'Address blocked.' }))
    } catch (err) {
      toast.error(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <AdminLayout title={t({ id: 'eunha.admin.canonical_email_blocks.email_addresses', defaultMessage: 'Email addresses' })} permission="manage_blocks" sub={BLOCK_TABS}>
      <div className="mb-4 grid gap-3 sm:grid-cols-2">
        <form onSubmit={test} className="space-y-2 rounded-lg border p-3">
          <h2 className="text-sm font-semibold">{t({ id: 'eunha.admin.canonical_email_blocks.test_an_address', defaultMessage: 'Test an address' })}</h2>
          <div className="flex gap-2">
            <Input
              type="email"
              aria-label={t({ id: 'eunha.admin.canonical_email_blocks.address_to_test', defaultMessage: 'Address to test' })}
              placeholder="someone@example.com"
              value={testEmail}
              onChange={(e) => {
                setTestEmail(e.target.value)
                setTestResult(null)
              }}
            />
            <Button type="submit" variant="outline" disabled={!testEmail.trim()}>
              {t({ id: 'eunha.admin.canonical_email_blocks.test', defaultMessage: 'Test' })}
            </Button>
          </div>
          {testResult && <p className="text-sm">{testResult}</p>}
        </form>
        <form onSubmit={add} className="space-y-2 rounded-lg border p-3">
          <h2 className="text-sm font-semibold">{t({ id: 'eunha.admin.canonical_email_blocks.block_an_address', defaultMessage: 'Block an address' })}</h2>
          <ChoiceSelect label={t({ id: 'eunha.admin.canonical_email_blocks.block_by', defaultMessage: 'Block by' })} value={mode} items={MODES} onChange={setMode} />
          <div className="flex gap-2">
            <Input
              aria-label={mode === 'email' ? t({ id: 'eunha.admin.canonical_email_blocks.address_to_block', defaultMessage: 'Address to block' }) : t({ id: 'eunha.admin.canonical_email_blocks.hash_to_block', defaultMessage: 'Hash to block' })}
              placeholder={mode === 'email' ? 'someone@example.com' : t({ id: 'eunha.admin.canonical_email_blocks.sha_256_hex_digest', defaultMessage: 'SHA-256 hex digest' })}
              value={value}
              onChange={(e) => setValue(e.target.value)}
            />
            <Button type="submit" disabled={saving || !value.trim()}>
              {t({ id: 'account.block_short', defaultMessage: 'Block' })}
            </Button>
          </div>
        </form>
      </div>
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <RowsSkeleton action />
      )}
      <div className={feed.items?.length ? 'divide-y rounded-lg border' : ''}>
        {feed.items?.map((b) => (
          <div key={b.id} className="flex items-center gap-2 p-2.5">
            <span className="min-w-0 flex-1 truncate font-mono text-xs">
              {b.canonical_email_hash}
            </span>
            <ConfirmButton
              size="xs"
              title={t({ id: 'eunha.admin.canonical_email_blocks.unblock_this_address', defaultMessage: 'Unblock this address?' })}
              description={t({ id: 'eunha.admin.canonical_email_blocks.whoever_owns_it_can_sign_up', defaultMessage: 'Whoever owns it can sign up again.' })}
              confirmLabel={t({ id: 'account.unblock_short', defaultMessage: 'Unblock' })}
              onConfirm={async () => {
                await deleteCanonicalEmailBlock(token ?? '', b.id)
                feed.mutate((items) => items.filter((x) => x.id !== b.id))
                toast.success(t({ id: 'eunha.admin.canonical_email_blocks.address_unblocked', defaultMessage: 'Address unblocked.' }))
              }}
            >
              {t({ id: 'account.unblock_short', defaultMessage: 'Unblock' })}
            </ConfirmButton>
          </div>
        ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">{t({ id: 'eunha.admin.canonical_email_blocks.no_addresses_are_blocked', defaultMessage: 'No addresses are blocked.' })}</p>
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
