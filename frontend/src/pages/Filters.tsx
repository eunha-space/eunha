import { useState } from 'react'
import { toast } from 'sonner'
import { beginLogin, getToken } from '../auth.ts'
import { restClient, type mastodon } from '../masto.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select.tsx'
import { errorMessage } from '@/lib/utils.ts'

const contexts = { home: 'Home and lists', notifications: 'Notifications', public: 'Public timelines', thread: 'Threads', account: 'Profiles' }
const actions = { warn: 'Show a warning', hide: 'Hide completely', blur: 'Blur media' }
const expirations = { keep: 'Keep current expiry', '0': 'Never', '1800': '30 minutes', '3600': '1 hour', '86400': '1 day', '604800': '7 days' }
type Keyword = { id?: string; keyword: string; wholeWord: boolean }

function FilterForm({ token, filter, onSaved, onCancel }: {
  token: string
  filter?: mastodon.v2.Filter
  onSaved: () => void
  onCancel: () => void
}) {
  const [title, setTitle] = useState(filter?.title ?? '')
  const [context, setContext] = useState<mastodon.v2.FilterContext[]>(filter?.context ?? ['home'])
  const [action, setAction] = useState<mastodon.v2.FilterAction>(filter?.filterAction ?? 'warn')
  const [expiry, setExpiry] = useState(filter ? 'keep' : '0')
  const [keywords, setKeywords] = useState<Keyword[]>(filter?.keywords ?? [{ keyword: '', wholeWord: true }])
  const [busy, setBusy] = useState(false)

  const save = async () => {
    setBusy(true)
    try {
      const expiresIn = expiry === 'keep' && filter?.expiresAt
        ? Math.ceil((Date.parse(filter.expiresAt) - Date.now()) / 1000)
        : Number(expiry) || undefined
      const present = keywords.filter((entry) => entry.keyword.trim())
      const entries = present.map((entry) => ({ ...entry, keyword: entry.keyword.trim() }))
      const resource = restClient(token).v2.filters
      const data = { title: title.trim(), context, filterAction: action, expiresIn }
      if (filter) {
        const removed = filter.keywords.filter(({ id }) => !present.some((entry) => entry.id === id))
        await resource.$select(filter.id).update({
          ...data,
          keywordsAttributes: [...entries, ...removed.map(({ id }) => ({ id, _destroy: true }))],
        })
      } else {
        await resource.create({ ...data, keywordsAttributes: entries })
      }
      onSaved()
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setBusy(false)
    }
  }

  return (
    <form className="space-y-3 rounded-md border p-3" onSubmit={(event) => { event.preventDefault(); void save() }}>
      <h2 className="font-semibold">{filter ? 'Edit filter' : 'New filter'}</h2>
      <Input aria-label="Filter name" placeholder="Filter name" value={title} maxLength={256}
        onChange={(event) => setTitle(event.target.value)} />
      <fieldset className="space-y-1">
        <legend className="text-sm font-medium">Apply to</legend>
        {(Object.keys(contexts) as (keyof typeof contexts)[]).map((value) => (
          <Label key={value} className="font-normal">
            <Switch checked={context.includes(value)} onCheckedChange={(checked) =>
              setContext(checked ? [...context, value] : context.filter((entry) => entry !== value))} />
            {contexts[value]}
          </Label>
        ))}
      </fieldset>
      <div className="flex flex-wrap gap-2">
        <Select items={actions} value={action} onValueChange={(value) => { if (value) setAction(value) }}>
          <SelectTrigger aria-label="Filter action"><SelectValue /></SelectTrigger>
          <SelectContent><SelectGroup>{Object.entries(actions).map(([value, label]) => (
            <SelectItem key={value} value={value}>{label}</SelectItem>
          ))}</SelectGroup></SelectContent>
        </Select>
        <Select items={expirations} value={expiry} onValueChange={(value) => { if (value) setExpiry(value) }}>
          <SelectTrigger aria-label="Filter expiration"><SelectValue /></SelectTrigger>
          <SelectContent><SelectGroup>{Object.entries(expirations).filter(([value]) => filter || value !== 'keep').map(([value, label]) => (
            <SelectItem key={value} value={value}>{label}</SelectItem>
          ))}</SelectGroup></SelectContent>
        </Select>
      </div>
      <fieldset className="space-y-2">
        <legend className="text-sm font-medium">Keywords</legend>
        {keywords.map((entry, index) => {
          const replace = (next: Keyword) => setKeywords(keywords.map((item, i) => i === index ? next : item))
          return (
            <div key={entry.id ?? index} className="space-y-1">
              <div className="flex gap-2">
                <Input aria-label={`Keyword ${index + 1}`} value={entry.keyword}
                  onChange={(event) => replace({ ...entry, keyword: event.target.value })} />
                <Button type="button" variant="ghost" onClick={() => setKeywords(keywords.filter((_, i) => i !== index))}>
                  Remove
                </Button>
              </div>
              <Label className="font-normal">
                <Switch checked={entry.wholeWord} onCheckedChange={(wholeWord) => replace({ ...entry, wholeWord })} />
                Whole word
              </Label>
            </div>
          )
        })}
        <Button type="button" size="sm" variant="outline"
          onClick={() => setKeywords([...keywords, { keyword: '', wholeWord: true }])}>Add keyword</Button>
      </fieldset>
      <div className="flex gap-2">
        <Button type="submit" disabled={busy || !title.trim() || context.length === 0}>Save filter</Button>
        <Button type="button" variant="outline" disabled={busy} onClick={onCancel}>Cancel</Button>
      </div>
    </form>
  )
}

function ManageFilters({ token }: { token: string }) {
  const [revision, setRevision] = useState(0)
  const [editing, setEditing] = useState<mastodon.v2.Filter | 'new'>()
  const [deleting, setDeleting] = useState<string>()
  const [busy, setBusy] = useState(false)
  const resource = restClient(token).v2.filters
  const feed = useInfinitePaginator(() => resource.list(), [token, revision])

  const remove = async (id: string) => {
    setBusy(true)
    try {
      await resource.$select(id).remove()
      feed.mutate((filters) => filters.filter((filter) => filter.id !== id))
      setDeleting(undefined)
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setBusy(false)
    }
  }

  if (editing) return <FilterForm token={token} filter={editing === 'new' ? undefined : editing}
    onCancel={() => setEditing(undefined)} onSaved={() => {
      setEditing(undefined)
      setRevision((value) => value + 1)
    }} />

  return (
    <>
      <Button variant="outline" onClick={() => setEditing('new')}>New filter</Button>
      {feed.error && <p role="alert">{feed.error}</p>}
      {!feed.items && !feed.error && <p>Loading…</p>}
      {feed.items?.length === 0 && <p>No filters yet.</p>}
      {feed.items?.map((filter) => (
        <div key={filter.id} className="space-y-2 rounded-md border p-3">
          <h2 className="font-medium">{filter.title}</h2>
          <p className="text-muted-foreground text-sm">{filter.keywords.map(({ keyword }) => keyword).join(', ')}</p>
          {deleting === filter.id ? (
            <div className="flex items-center gap-2">
              <Button size="sm" variant="destructive" disabled={busy} onClick={() => void remove(filter.id)}>Confirm delete</Button>
              <Button size="sm" variant="ghost" onClick={() => setDeleting(undefined)}>Cancel</Button>
            </div>
          ) : (
            <div className="flex gap-2">
              <Button size="sm" variant="outline" onClick={() => setEditing(filter)}>Edit</Button>
              <Button size="sm" variant="ghost" onClick={() => setDeleting(filter.id)}>Delete</Button>
            </div>
          )}
        </div>
      ))}
    </>
  )
}

export default function Filters() {
  const token = getToken()
  return (
    <div className="page-frame space-y-3">
      <TopBar />
      <h1 className="text-xl font-bold">Filters</h1>
      {token ? <ManageFilters token={token} /> : <Button onClick={() => void beginLogin()}>Sign in</Button>}
    </div>
  )
}
