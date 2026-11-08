import { useState } from 'react'
import { Link } from 'react-router-dom'
import { toast } from 'sonner'
import { beginLogin, getToken } from '../auth.ts'
import { restClient } from '../masto.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { TopBar } from '@/components/top-bar.tsx'
import { errorMessage } from '@/lib/utils.ts'
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent,
  AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from '@/components/ui/alert-dialog.tsx'

function ManageDomainBlocks({ token }: { token: string }) {
  const [domain, setDomain] = useState('')
  const [confirm, setConfirm] = useState(false)
  const [busy, setBusy] = useState(false)
  const [revision, refresh] = useState(0)
  const client = restClient(token).v1.domainBlocks
  const feed = useInfinitePaginator(() => client.list({ limit: 40 }), [token, revision])

  const change = async (target: string, block: boolean) => {
    setBusy(true)
    try {
      if (block) {
        await client.create({ domain: target })
        setDomain('')
        refresh((value) => value + 1)
      } else {
        await client.remove({ domain: target })
        feed.mutate((items) => items.filter((item) => item !== target))
      }
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-3">
      <form className="flex gap-2" onSubmit={(event) => {
        event.preventDefault()
        if (domain.trim()) setConfirm(true)
      }}>
        <Input aria-label="Domain" placeholder="example.social" value={domain}
          onChange={(event) => setDomain(event.target.value)} />
        <Button type="submit" disabled={busy || !domain.trim()}>Block domain</Button>
      </form>
      {feed.error && <p role="alert" className="text-destructive">{feed.error}</p>}
      {!feed.items && !feed.error && <p>Loading…</p>}
      {feed.items?.length === 0 && <p>No blocked domains.</p>}
      {feed.items?.map((item) => (
        <div key={item} className="flex items-center justify-between gap-2 rounded-md border p-3">
          <span className="break-all">{item}</span>
          <Button size="sm" variant="outline" disabled={busy} onClick={() => void change(item, false)}>
            Unblock
          </Button>
        </div>
      ))}
      {feed.items && !feed.done && !feed.error && (
        <Button variant="outline" disabled={feed.loadingMore} onClick={feed.loadMore}>Load more</Button>
      )}
      <AlertDialog open={confirm} onOpenChange={setConfirm}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Block {domain.trim()}?</AlertDialogTitle>
            <AlertDialogDescription>
              Posts and notifications from this domain will be hidden and followers from it will be removed.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction onClick={() => void change(domain.trim(), true)}>Block domain</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  )
}

export default function DomainBlocks() {
  const token = getToken()
  return (
    <div className="page-frame space-y-3">
      <TopBar />
      <Link to="/blocked">Blocked accounts</Link>
      <h1 className="text-xl font-bold">Blocked domains</h1>
      {token ? <ManageDomainBlocks token={token} /> : <Button onClick={() => void beginLogin()}>Sign in</Button>}
    </div>
  )
}
