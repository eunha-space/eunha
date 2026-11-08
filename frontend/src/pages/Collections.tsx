import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { toast } from 'sonner'
import { lookupAccount, type AccountCollection } from '../api.ts'
import { beginLogin, getActiveAccountId, getToken } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { errorMessage } from '@/lib/utils.ts'

type Collection = AccountCollection & {
  account_id: string
  sensitive: boolean
  local: boolean
  items: { id: string; state: string; account_id?: string }[]
}
type AccountLabel = { id: string; acct: string; display_name: string }
type Detail = { collection: Collection; accounts: AccountLabel[] }

async function request<T>(path: string, token: string | null, method = 'GET', body?: object): Promise<T> {
  const response = await fetch(`/api/v1${path}`, {
    method,
    headers: {
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...(body ? { 'Content-Type': 'application/json' } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  })
  if (!response.ok) {
    const problem = await response.json().catch(() => null) as { error?: string } | null
    throw new Error(problem?.error ?? 'Could not complete the collection request')
  }
  if (method === 'DELETE') return undefined as T
  return response.json() as Promise<T>
}

function CollectionEditor({ token, collection, onSaved }: {
  token: string
  collection?: Collection
  onSaved: (id: string) => void
}) {
  const [name, setName] = useState(collection?.name ?? '')
  const [description, setDescription] = useState(collection?.description ?? '')
  const [discoverable, setDiscoverable] = useState(collection?.discoverable ?? false)
  const [sensitive, setSensitive] = useState(collection?.sensitive ?? false)
  const [busy, setBusy] = useState(false)

  const save = async () => {
    setBusy(true)
    try {
      const result = await request<{ collection: Collection }>(
        collection ? `/collections/${collection.id}` : '/collections',
        token, collection ? 'PATCH' : 'POST', { name: name.trim(), description, discoverable, sensitive },
      )
      onSaved(result.collection.id)
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setBusy(false)
    }
  }

  return (
    <form className="space-y-2 rounded-md border p-3" onSubmit={(event) => { event.preventDefault(); void save() }}>
      <h2 className="font-semibold">{collection ? 'Edit collection' : 'New collection'}</h2>
      <Input aria-label="Collection name" placeholder="Collection name" value={name}
        onChange={(event) => setName(event.target.value)} />
      <Textarea aria-label="Collection description" placeholder="Description" value={description}
        onChange={(event) => setDescription(event.target.value)} />
      <Label><Switch checked={discoverable} onCheckedChange={setDiscoverable} /> Show on my profile</Label>
      <Label><Switch checked={sensitive} onCheckedChange={setSensitive} /> Mark as sensitive</Label>
      <Button type="submit" disabled={busy || !name.trim()}>{collection ? 'Save changes' : 'Create collection'}</Button>
    </form>
  )
}

function CollectionIndex({ token, accountId }: { token: string; accountId: string }) {
  const navigate = useNavigate()
  const [items, setItems] = useState<AccountCollection[]>()
  const [error, setError] = useState(false)
  useEffect(() => {
    let active = true
    request<{ collections: AccountCollection[] }>(`/accounts/${accountId}/collections`, token)
      .then(({ collections }) => { if (active) setItems(collections) })
      .catch(() => { if (active) setError(true) })
    return () => { active = false }
  }, [accountId, token])
  return (
    <>
      <h1 className="text-xl font-bold">Collections</h1>
      <CollectionEditor token={token} onSaved={(id) => navigate(`/collections/${id}`)} />
      {error && <p role="alert">Could not load collections.</p>}
      {!items && !error && <p>Loading…</p>}
      {items?.map((collection) => (
        <Link key={collection.id} to={`/collections/${collection.id}`} className="block rounded-md border p-3">
          {collection.name}
        </Link>
      ))}
    </>
  )
}

function CollectionDetail({ id, token, accountId }: { id: string; token: string | null; accountId: string | null }) {
  const navigate = useNavigate()
  const [detail, setDetail] = useState<Detail>()
  const [revision, setRevision] = useState(0)
  const [error, setError] = useState(false)
  const [acct, setAcct] = useState('')
  const [busy, setBusy] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [showSensitive, setShowSensitive] = useState(false)

  useEffect(() => {
    let active = true
    setError(false)
    request<Detail>(`/collections/${id}`, token).then((value) => {
      if (active) setDetail(value)
    }).catch(() => { if (active) setError(true) })
    return () => { active = false }
  }, [id, token, revision])

  const run = async (action: () => Promise<unknown>) => {
    setBusy(true)
    try {
      await action()
      setRevision((value) => value + 1)
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setBusy(false)
    }
  }

  if (!detail) return <p role={error ? 'alert' : undefined}>{error ? 'Could not load this collection.' : 'Loading…'}</p>
  const { collection, accounts } = detail
  const owner = token && collection.local && accountId === collection.account_id
  return (
    <>
      <h1 className="text-xl font-bold">{collection.name}</h1>
      {error && <p role="alert">Could not refresh this collection.</p>}
      {accounts[0] && <Link to={`/@${accounts[0].acct}`}>By @{accounts[0].acct}</Link>}
      {collection.sensitive && !owner && !showSensitive ? (
        <Button variant="outline" onClick={() => setShowSensitive(true)}>Show sensitive collection</Button>
      ) : (
        <>
          {collection.description && <p className="whitespace-pre-wrap">{collection.description}</p>}
          {collection.items.map((item) => {
            const account = accounts.find(({ id: candidate }) => candidate === item.account_id)
            return (
              <div key={item.id} className="flex items-center justify-between gap-2 rounded-md border p-3">
                <div>
                  {account ? <Link to={`/@${account.acct}`}>{account.display_name || `@${account.acct}`}</Link>
                    : <span>Unavailable account</span>}
                  {item.state !== 'accepted' && <p className="text-muted-foreground text-xs">{item.state}</p>}
                </div>
                {owner && <Button size="sm" variant="outline" disabled={busy} onClick={() => void run(() =>
                  request(`/collections/${id}/items/${item.id}`, token, 'DELETE'))}>Remove</Button>}
              </div>
            )
          })}
        </>
      )}
      {owner && token && (
        <>
          <CollectionEditor token={token} collection={collection} onSaved={() => setRevision((value) => value + 1)} />
          <form className="flex gap-2" onSubmit={(event) => {
            event.preventDefault()
            void run(async () => {
              const account = await lookupAccount(acct.trim().replace(/^@/, ''), token)
              await request(`/collections/${id}/items`, token, 'POST', { account_id: account.id })
              setAcct('')
            })
          }}>
            <Input aria-label="Account handle" placeholder="@someone@example.social" value={acct}
              onChange={(event) => setAcct(event.target.value)} />
            <Button type="submit" disabled={busy || !acct.trim()}>Add account</Button>
          </form>
          {confirmDelete ? (
            <div className="flex gap-2">
              <Button variant="destructive" disabled={busy} onClick={() => void run(async () => {
                await request(`/collections/${id}`, token, 'DELETE')
                navigate('/collections')
              })}>Confirm delete collection</Button>
              <Button variant="outline" onClick={() => setConfirmDelete(false)}>Cancel</Button>
            </div>
          ) : <Button variant="outline" onClick={() => setConfirmDelete(true)}>Delete collection</Button>}
        </>
      )}
    </>
  )
}

export default function Collections() {
  const token = getToken()
  const accountId = getActiveAccountId()
  const { id } = useParams()
  return (
    <div className="page-frame space-y-3">
      <TopBar />
      {id ? /^\d+$/.test(id)
        ? <CollectionDetail key={id} id={id} token={token} accountId={accountId} />
        : <p>Collection unavailable.</p>
        : token && accountId ? <CollectionIndex token={token} accountId={accountId} />
          : <Button onClick={() => void beginLogin()}>Sign in</Button>}
    </div>
  )
}
