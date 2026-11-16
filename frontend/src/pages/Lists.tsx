import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { toast } from 'sonner'
import { lookupAccount } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { restClient, type mastodon } from '../masto.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { AccountRow } from '@/components/account-row.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select.tsx'
import { errorMessage } from '@/lib/utils.ts'
import { FeedSkeleton, RowsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const replies = { get list() { return t({ id: 'eunha.lists.list_members', defaultMessage: 'List members' }) }, get followed() { return t({ id: 'eunha.lists.anyone_i_follow', defaultMessage: 'Anyone I follow' }) }, get none() { return t({ id: 'eunha.lists.no_replies', defaultMessage: 'No replies' }) } }

function ListIndex({ token }: { token: string }) {
  const navigate = useNavigate()
  const [title, setTitle] = useState('')
  const [busy, setBusy] = useState(false)
  const feed = useInfinitePaginator(() => restClient(token).v1.lists.list(), [token])

  const create = async () => {
    if (!title.trim()) return
    setBusy(true)
    try {
      const list = await restClient(token).v1.lists.create({ title: title.trim() })
      navigate(`/lists/${list.id}`)
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <h1 className="text-xl font-bold">{t({ id: 'column.lists', defaultMessage: 'Lists' })}</h1>
      <form className="flex gap-2" onSubmit={(event) => { event.preventDefault(); void create() }}>
        <Input value={title} aria-label={t({ id: 'eunha.lists.list_name', defaultMessage: 'List name' })} placeholder={t({ id: 'eunha.lists.list_name', defaultMessage: 'List name' })} maxLength={100}
          onChange={(event) => setTitle(event.target.value)} />
        <Button type="submit" disabled={busy || !title.trim()}>{t({ id: 'lists.create_list', defaultMessage: 'Create list' })}</Button>
      </form>
      {feed.error && <p role="alert">{feed.error}</p>}
      {!feed.items && !feed.error && <RowsSkeleton count={3} />}
      {feed.items?.length === 0 && <p>{t({ id: 'eunha.lists.you_haven_t_created_any_lists', defaultMessage: 'You haven’t created any lists.' })}</p>}
      {feed.items?.map((list) => (
        <Link key={list.id} to={`/lists/${list.id}`} className="block rounded-md border p-3">{list.title}</Link>
      ))}
    </>
  )
}

function ListDetail({ id, token }: { id: string; token: string }) {
  const navigate = useNavigate()
  const client = restClient(token).v1
  const resource = client.lists.$select(id)
  const [list, setList] = useState<mastodon.v1.List>()
  const [error, setError] = useState(false)
  const [manage, setManage] = useState(false)
  const [busy, setBusy] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [acct, setAcct] = useState('')
  const [revision, setRevision] = useState(0)
  const members = useInfinitePaginator(() => resource.accounts.list({ limit: 40 }), [id, token, revision])
  const posts = useInfinitePaginator(() => client.timelines.list.$select(id).list({ limit: 40 }), [id, token, revision])

  useEffect(() => {
    let active = true
    restClient(token).v1.lists.$select(id).fetch()
      .then((value) => { if (active) setList(value) })
      .catch(() => { if (active) setError(true) })
    return () => { active = false }
  }, [id, token])

  const run = async (action: () => Promise<void>) => {
    setBusy(true)
    try { await action() } catch (error) { toast.error(errorMessage(error)) }
    finally { setBusy(false) }
  }

  const add = () => run(async () => {
    const account = await lookupAccount(acct.trim().replace(/^@/, ''), token)
    await resource.accounts.create({ accountIds: [account.id] })
    setAcct('')
    setRevision((value) => value + 1)
  })

  if (!list) return error ? <p role="alert">{t({ id: 'eunha.lists.could_not_load_this_list', defaultMessage: 'Could not load this list.' })}</p> : <FeedSkeleton />
  return (
    <>
      <div className="flex items-center justify-between gap-2">
        <h1 className="text-xl font-bold">{list.title}</h1>
        <Button variant="outline" onClick={() => setManage(!manage)}>{manage ? t({ id: 'eunha.lists.view_timeline', defaultMessage: 'View timeline' }) : t({ id: 'eunha.lists.manage_list', defaultMessage: 'Manage list' })}</Button>
      </div>
      {manage ? (
        <>
          <form className="space-y-2 rounded-md border p-3" onSubmit={(event) => {
            event.preventDefault()
            void run(async () => {
              const { title, exclusive, repliesPolicy } = list
              const changes = { title: title.trim(), exclusive, repliesPolicy }
              setList(await resource.update(changes))
              setRevision((value) => value + 1)
            })
          }}>
            <Input aria-label={t({ id: 'eunha.lists.list_name', defaultMessage: 'List name' })} value={list.title} maxLength={100}
              onChange={(event) => setList({ ...list, title: event.target.value })} />
            <Select items={replies} value={list.repliesPolicy} onValueChange={(value) => {
              if (value) setList({ ...list, repliesPolicy: value })
            }}>
              <SelectTrigger aria-label={t({ id: 'eunha.lists.show_replies_to', defaultMessage: 'Show replies to' })}><SelectValue /></SelectTrigger>
              <SelectContent><SelectGroup>{Object.entries(replies).map(([value, label]) => (
                <SelectItem key={value} value={value}>{label}</SelectItem>
              ))}</SelectGroup></SelectContent>
            </Select>
            <Label><Switch checked={list.exclusive} onCheckedChange={(exclusive) => setList({ ...list, exclusive })} />
              {t({ id: 'eunha.lists.hide_list_members_from_home', defaultMessage: 'Hide list members from Home' })}
            </Label>
            <Button type="submit" disabled={busy || !list.title.trim()}>{t({ id: 'eunha.lists.save_list', defaultMessage: 'Save list' })}</Button>
          </form>
          <form className="flex gap-2" onSubmit={(event) => { event.preventDefault(); void add() }}>
            <Input aria-label={t({ id: 'eunha.lists.account_handle', defaultMessage: 'Account handle' })} placeholder="@someone@example.social" value={acct}
              onChange={(event) => setAcct(event.target.value)} />
            <Button type="submit" disabled={busy || !acct.trim()}>{t({ id: 'eunha.lists.add_member', defaultMessage: 'Add member' })}</Button>
          </form>
          {members.error && <p role="alert">{members.error}</p>}
          {members.items?.map((account) => (
            <AccountRow key={account.id} account={account} action={
              <Button size="sm" variant="outline" disabled={busy} onClick={() => void run(async () => {
                await resource.accounts.remove({ accountIds: [account.id] })
                setRevision((value) => value + 1)
              })}>{t({ id: 'tag.remove', defaultMessage: 'Remove' })}</Button>
            } />
          ))}
          {members.items && !members.done && !members.error && (
            <Button variant="outline" disabled={members.loadingMore} onClick={members.loadMore}>{t({ id: 'eunha.lists.load_more_members', defaultMessage: 'Load more members' })}</Button>
          )}
          {confirmDelete ? (
            <div className="flex flex-wrap items-center gap-2">
              <span>{t({ id: 'eunha.lists.delete_this_list', defaultMessage: 'Delete this list?' })}</span>
              <Button variant="destructive" disabled={busy} onClick={() => void run(async () => {
                await resource.remove()
                navigate('/lists')
              })}>{t({ id: 'eunha.lists.confirm_delete', defaultMessage: 'Confirm delete' })}</Button>
              <Button variant="outline" onClick={() => setConfirmDelete(false)}>{t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}</Button>
            </div>
          ) : <Button variant="outline" onClick={() => setConfirmDelete(true)}>{t({ id: 'lists.delete', defaultMessage: 'Delete list' })}</Button>}
        </>
      ) : (
        <>
          {posts.error && <p role="alert">{posts.error}</p>}
          {!posts.items && !posts.error && <FeedSkeleton />}
          {posts.items?.length === 0 && <p>{t({ id: 'eunha.lists.no_posts_in_this_list_yet', defaultMessage: 'No posts in this list yet.' })}</p>}
          {posts.items?.map((post) => (
            <StatusCard key={post.id} status={post.reblog ?? post} token={token}
              boostedBy={post.reblog ? post.account : undefined} filterContext="home" />
          ))}
          {posts.items && (
            <InfiniteScroll onLoadMore={posts.loadMore} loading={posts.loadingMore}
              done={posts.done} hasItems={posts.items.length > 0} />
          )}
        </>
      )}
    </>
  )
}

export default function Lists() {
  const token = getToken()
  const { id } = useParams()
  return (
    <div className="page-frame space-y-3">
      <TopBar />
      {id && <Link to="/lists">{t({ id: 'eunha.lists.all_lists', defaultMessage: 'All lists' })}</Link>}
      {token ? id ? <ListDetail key={id} id={id} token={token} /> : <ListIndex token={token} />
        : <Button onClick={() => void beginLogin()}>{t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}</Button>}
    </div>
  )
}
