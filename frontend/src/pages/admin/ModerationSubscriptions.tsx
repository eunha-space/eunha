import { useCallback, useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'
import { Link, useNavigate, useParams } from 'react-router-dom'

import {
  applyModerationSuggestion,
  createModerationSubscription,
  deleteModerationSubscription,
  dismissModerationSuggestion,
  getModerationSubscription,
  listModerationSubscriptionLogs,
  listModerationSubscriptions,
  listModerationSuggestions,
  updateModerationSubscription,
  type AdvisoryAction,
  type ApplyConditions,
  type ListAction,
  type ModerationSubscription,
  type ModerationSubscriptionForm,
  type SuggestionAction,
  type SuggestionTarget,
} from '../../admin-server-api.ts'
import type { ActionLog } from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { ActionLogEntry } from '@/components/admin/action-log-entry.tsx'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { ChoiceSelect, ConfirmButton, formatDate } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'

const TABS = [
  { to: '/admin/moderation_subscriptions', label: 'Subscriptions' },
  { to: '/admin/moderation_suggestions', label: 'Suggestions' },
  { to: '/admin/moderation_subscriptions/logs', label: 'Logs' },
]

/** `admin.moderation_subscriptions.list_actions`. */
const LIST_ACTIONS: Record<ListAction | 'default', string> = {
  default: 'Mixed (Allow & Deny)',
  accept: 'Allow-list',
  reject: 'Block-list',
}

const LIST_ACTION_HINTS: Record<ListAction | 'default', string> = {
  default:
    'Items with no specified action will be treated as blocks. Blocks and allows will both be treated as such.',
  accept: 'Items with no specified action will be treated as allowed. Blocks will be ignored.',
  reject: 'Items with no specified action will be treated as blocks. Allows will be ignored.',
}

/** `admin.moderation_subscriptions.apply_conditions`. */
const APPLY_CONDITIONS: Record<ApplyConditions, string> = {
  never: 'Never',
  safely: 'Only if it does not break relationships',
  always: 'Always',
}

const APPLY_CONDITION_HINTS: Record<ApplyConditions, string> = {
  never: 'Never apply recommended actions automatically. Always generate a suggestion instead.',
  safely:
    'Apply recommended actions unless they would break existing follow relationships with local users. In such cases, a suggestion will be generated instead.',
  always:
    'Always apply recommended actions automatically. Only use this option for lists you fully trust.',
}

/** `admin.moderation_subscriptions.advisory_actions`. */
const ADVISORY_ACTIONS: Record<AdvisoryAction, string> = {
  accept: 'allow',
  limit: 'silence',
  reject: 'suspend',
}

/** `admin.moderation_suggestions.apply_*`. */
const APPLY_LABELS: Record<SuggestionAction, string> = {
  accept: 'Allow',
  limit: 'Limit',
  reject: 'Suspend',
  retract: 'Retract',
}

/** `admin.moderation_suggestions.current_states`. */
const CURRENT_STATES: Record<AdvisoryAction, string> = {
  accept: 'currently allowed',
  limit: 'currently limited',
  reject: 'currently suspended',
}

function SubscriptionFields({
  initial,
  submitLabel,
  onSubmit,
}: {
  initial?: ModerationSubscription
  submitLabel: string
  onSubmit: (form: ModerationSubscriptionForm) => Promise<void>
}) {
  const [name, setName] = useState(initial?.name ?? '')
  const [url, setUrl] = useState(initial?.url ?? '')
  const [priority, setPriority] = useState(String(initial?.priority ?? 0))
  const [listAction, setListAction] = useState<ListAction | 'default'>(
    initial?.list_action ?? 'default',
  )
  const [conditions, setConditions] = useState<ApplyConditions>(
    initial?.apply_conditions ?? 'never',
  )
  const [retract, setRetract] = useState(initial?.retract_automatically ?? true)
  const [override, setOverride] = useState(initial?.override_unattributed ?? false)
  const [busy, setBusy] = useState(false)

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setBusy(true)
    try {
      await onSubmit({
        name,
        url,
        priority: Number(priority),
        list_action: listAction === 'default' ? '' : listAction,
        apply_conditions: conditions,
        retract_automatically: retract,
        override_unattributed: override,
      })
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setBusy(false)
    }
  }

  return (
    <form onSubmit={(e) => void submit(e)} className="space-y-3 rounded-lg border p-3">
      <div className="space-y-1">
        <Label htmlFor="subscription-name">Name</Label>
        <Input id="subscription-name" value={name} onChange={(e) => setName(e.target.value)} />
      </div>
      <div className="space-y-1">
        <Label htmlFor="subscription-url">URL</Label>
        <Input
          id="subscription-url"
          type="url"
          value={url}
          onChange={(e) => setUrl(e.target.value)}
        />
      </div>
      <div className="space-y-1">
        <Label htmlFor="subscription-priority">Priority</Label>
        <Input
          id="subscription-priority"
          type="number"
          value={priority}
          onChange={(e) => setPriority(e.target.value)}
        />
        <p className="text-muted-foreground text-xs">
          Moderation subscriptions are evaluated in order of priority, subscriptions with a lower
          number override subscriptions with a higher one.
        </p>
      </div>
      <div className="space-y-1">
        <Label>Subscription type</Label>
        <ChoiceSelect
          label="Subscription type"
          value={listAction}
          items={LIST_ACTIONS}
          onChange={setListAction}
        />
        <p className="text-muted-foreground text-xs">{LIST_ACTION_HINTS[listAction]}</p>
      </div>
      <div className="space-y-1">
        <Label>Apply advisories automatically</Label>
        <ChoiceSelect
          label="Apply advisories automatically"
          value={conditions}
          items={APPLY_CONDITIONS}
          onChange={setConditions}
        />
        <p className="text-muted-foreground text-xs">{APPLY_CONDITION_HINTS[conditions]}</p>
      </div>
      <div className="flex items-start gap-2">
        <Checkbox
          id="subscription-retract"
          checked={retract}
          onCheckedChange={(checked) => setRetract(checked === true)}
        />
        <Label htmlFor="subscription-retract" className="block">
          Retract advisories automatically
          <span className="text-muted-foreground block text-xs font-normal">
            Whether to allow retracting or downgrading policy changes when the subscription removes
            a corresponding item.
          </span>
        </Label>
      </div>
      <div className="flex items-start gap-2">
        <Checkbox
          id="subscription-override"
          checked={override}
          onCheckedChange={(checked) => setOverride(checked === true)}
        />
        <Label htmlFor="subscription-override" className="block">
          Override manual blocks
          <span className="text-muted-foreground block text-xs font-normal">
            Allow this subscription to automatically upgrade or take over existing manual blocks
            when automatically applying advisories.
          </span>
        </Label>
      </div>
      <Button type="submit" size="sm" disabled={busy}>
        {submitLabel}
      </Button>
    </form>
  )
}

/** `Admin::ModerationSubscriptionsController#index` and `#new`. */
export function ModerationSubscriptions() {
  const token = getToken()
  const [subscriptions, setSubscriptions] = useState<ModerationSubscription[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [adding, setAdding] = useState(false)

  const load = useCallback(() => {
    if (!token) return
    listModerationSubscriptions(token)
      .then(setSubscriptions)
      .catch((e) => setError(errorMessage(e)))
  }, [token])

  useEffect(load, [load])

  return (
    <AdminLayout
      title="Moderation subscriptions"
      permission="manage_federation"
      sub={TABS}
      actions={
        <Button size="sm" onClick={() => setAdding((open) => !open)}>
          Add subscription
        </Button>
      }
    >
      <AdminError error={error} />
      {adding && (
        <SubscriptionFields
          submitLabel="Subscribe"
          onSubmit={async (form) => {
            await createModerationSubscription(token ?? '', form)
            setAdding(false)
            load()
          }}
        />
      )}
      {subscriptions?.length === 0 && (
        <p className="text-muted-foreground text-sm">
          No moderation list has been subscribed to yet.
        </p>
      )}
      <div className="space-y-2">
        {subscriptions?.map((subscription) => (
          <div key={subscription.id} className="space-y-1 rounded-lg border p-3">
            <div className="flex flex-wrap items-center gap-2">
              <Link
                to={`/admin/moderation_subscriptions/${subscription.id}`}
                className="text-sm font-medium"
              >
                {subscription.name}
              </Link>
              <Badge variant="outline">{LIST_ACTIONS[subscription.list_action ?? 'default']}</Badge>
              <Badge variant="outline">Priority {subscription.priority}</Badge>
            </div>
            <p className="text-muted-foreground text-xs break-all">{subscription.url}</p>
            <p className="text-muted-foreground text-xs">
              {subscription.last_synced_at
                ? `Last synced at ${formatDate(subscription.last_synced_at)}`
                : 'Not synced yet'}
            </p>
          </div>
        ))}
      </div>
    </AdminLayout>
  )
}

/** `Admin::ModerationSubscriptionsController#show` and `#edit`. */
export function ModerationSubscriptionDetail() {
  const { id = '' } = useParams()
  const token = getToken()
  const navigate = useNavigate()
  const [page, setPage] = useState(1)
  const [data, setData] = useState<Awaited<
    ReturnType<typeof getModerationSubscription>
  > | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [editing, setEditing] = useState(false)

  const load = useCallback(() => {
    if (!token) return
    getModerationSubscription(token, id, page)
      .then(setData)
      .catch((e) => setError(errorMessage(e)))
  }, [token, id, page])

  useEffect(load, [load])

  const subscription = data?.subscription
  return (
    <AdminLayout
      title={subscription ? `Moderation subscription: ${subscription.name}` : 'Moderation subscription'}
      permission="manage_federation"
      sub={TABS}
    >
      <AdminError error={error} />
      {subscription && data && (
        <div className="space-y-3">
          <p className="text-muted-foreground text-sm break-all">{subscription.url}</p>
          <div className="flex flex-wrap gap-2">
            <Button size="sm" variant="outline" onClick={() => setEditing((open) => !open)}>
              Edit subscription
            </Button>
            <ConfirmButton
              title="Delete this subscription?"
              description="Are you sure you want to delete this moderation subscription?"
              confirmLabel="Delete"
              onConfirm={async () => {
                await deleteModerationSubscription(token ?? '', id)
                toast.success('Successfully deleted moderation subscription')
                void navigate('/admin/moderation_subscriptions')
              }}
            >
              Delete
            </ConfirmButton>
          </div>
          {editing && (
            <SubscriptionFields
              initial={subscription}
              submitLabel="Save changes"
              onSubmit={async (form) => {
                await updateModerationSubscription(token ?? '', id, form)
                setEditing(false)
                load()
              }}
            />
          )}
          {data.advisories.length === 0 && (
            <p className="text-muted-foreground text-sm">This subscription has no advisory yet.</p>
          )}
          <ul className="divide-y rounded-lg border">
            {data.advisories.map((advisory) => (
              <li key={advisory.id} className="flex items-center justify-between gap-2 p-2 text-sm">
                <Link to={`/admin/instances/${advisory.target_key}`}>{advisory.target_key}</Link>
                <span className="text-muted-foreground text-xs">
                  Action: {ADVISORY_ACTIONS[advisory.action]}
                </span>
              </li>
            ))}
          </ul>
          <div className="flex gap-2">
            <Button
              size="sm"
              variant="outline"
              disabled={page <= 1}
              onClick={() => setPage((p) => p - 1)}
            >
              Previous
            </Button>
            <Button
              size="sm"
              variant="outline"
              disabled={page * data.page_size >= data.advisories_count}
              onClick={() => setPage((p) => p + 1)}
            >
              Next
            </Button>
          </div>
        </div>
      )}
    </AdminLayout>
  )
}

/** `Admin::ModerationSubscriptionsController#logs`. */
export function ModerationSubscriptionLogs() {
  const token = getToken()
  const [page, setPage] = useState(1)
  const [logs, setLogs] = useState<ActionLog[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!token) return
    listModerationSubscriptionLogs(token, page)
      .then(setLogs)
      .catch((e) => setError(errorMessage(e)))
  }, [token, page])

  return (
    <AdminLayout title="Moderation subscriptions logs" permission="manage_federation" sub={TABS}>
      <AdminError error={error} />
      {logs?.length === 0 && <p className="text-muted-foreground text-sm">No logs found.</p>}
      <div className="divide-y rounded-lg border">
        {logs?.map((log) => <ActionLogEntry key={log.id} log={log} />)}
      </div>
      <div className="mt-3 flex gap-2">
        <Button
          size="sm"
          variant="outline"
          disabled={page <= 1}
          onClick={() => setPage((p) => p - 1)}
        >
          Previous
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={(logs?.length ?? 0) === 0}
          onClick={() => setPage((p) => p + 1)}
        >
          Next
        </Button>
      </div>
    </AdminLayout>
  )
}

/** `_moderation_suggestions`: what is advised on one domain, and acting on it. */
export function SuggestionCard({
  target,
  onChanged,
}: {
  target: SuggestionTarget
  onChanged: () => void
}) {
  const token = getToken() ?? ''
  const waiting = target.suggestions.filter(
    (suggestion) => suggestion.state === 'new' || suggestion.state === 'mailed',
  )

  const apply = async (id: string, confirmations = {}) => {
    const { result, target_key: key } = await applyModerationSuggestion(token, id, confirmations)
    switch (result) {
      case 'applied':
        toast.success(`Successfully applied suggestion for ${key}`)
        onChanged()
        return
      case 'retracted':
        toast.success(`Successfully retracted policy for ${key}`)
        onChanged()
        return
      case 'unable_to_apply':
        toast.error(`Unable to apply suggestion for ${key}`)
        return
      default:
        // Each confirmation is given by its button; asked for again, the block
        // changed since the page was read.
        toast.error(`Please confirm again for ${key}`)
        onChanged()
    }
  }

  return (
    <div className="space-y-2 rounded-lg border p-3">
      <div className="flex flex-wrap items-center gap-2">
        <Link to={`/admin/instances/${target.target_key}`} className="text-sm font-medium">
          {target.target_key}
        </Link>
        <Badge variant="outline">{CURRENT_STATES[target.current_state]}</Badge>
      </div>
      {target.advisories.map((advisory, index) => (
        <p key={index} className="text-muted-foreground text-xs">
          Action: {ADVISORY_ACTIONS[advisory.action]}, suggested by{' '}
          <Link to={`/admin/moderation_subscriptions/${advisory.subscription.id}`}>
            {advisory.subscription.name}
          </Link>
        </p>
      ))}
      <div className="flex flex-wrap gap-2">
        {waiting.map((suggestion) => {
          const downgrade = target.current_state !== 'accept' && suggestion.action === 'limit'
          if (suggestion.action === 'reject' && target.current_state !== 'reject') {
            return (
              <ConfirmButton
                key={suggestion.id}
                title={`Suspend ${target.target_key}?`}
                description="This removes the domain's accounts and their content from this server, and breaks follows with them."
                confirmLabel="Suspend"
                onConfirm={() => apply(suggestion.id, { confirm: true })}
              >
                {APPLY_LABELS.reject}
              </ConfirmButton>
            )
          }
          if (downgrade) {
            return (
              <ConfirmButton
                key={suggestion.id}
                title={`Confirm domain block downgrade for ${target.target_key}`}
                description={`You are about to downgrade the existing block on ${target.target_key} and its subdomains.`}
                confirmLabel="Downgrade"
                onConfirm={() => apply(suggestion.id, { confirm_downgrade: true })}
              >
                {APPLY_LABELS.limit}
              </ConfirmButton>
            )
          }
          return (
            <Button
              key={suggestion.id}
              size="sm"
              variant="outline"
              onClick={() => void apply(suggestion.id).catch((e) => toast.error(errorMessage(e)))}
            >
              {APPLY_LABELS[suggestion.action]}
            </Button>
          )
        })}
        {waiting[0] && (
          <Button
            size="sm"
            variant="ghost"
            onClick={() =>
              void dismissModerationSuggestion(token, waiting[0].id)
                .then(() => {
                  toast.success(`Successfully dismissed suggestion for ${target.target_key}`)
                  onChanged()
                })
                .catch((e) => toast.error(errorMessage(e)))
            }
          >
            Dismiss suggestions
          </Button>
        )}
      </div>
    </div>
  )
}

/** `Admin::ModerationSuggestionsController#index`. */
export function ModerationSuggestions() {
  const token = getToken()
  const [targets, setTargets] = useState<SuggestionTarget[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(() => {
    if (!token) return
    listModerationSuggestions(token)
      .then(setTargets)
      .catch((e) => setError(errorMessage(e)))
  }, [token])

  useEffect(load, [load])

  return (
    <AdminLayout title="Moderation suggestions" permission="manage_federation" sub={TABS}>
      <AdminError error={error} />
      <p className="text-muted-foreground text-sm">
        Below are suggestions from your moderation subscriptions, ordered by priority.
      </p>
      {targets?.length === 0 && (
        <p className="text-muted-foreground text-sm">
          There is no moderation suggestion at this time.
        </p>
      )}
      <div className="mt-3 space-y-2">
        {targets?.map((target) => (
          <SuggestionCard key={target.target_key} target={target} onChanged={load} />
        ))}
      </div>
    </AdminLayout>
  )
}
