import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import type { mastodon } from '../masto.ts'
import { fileReport, getAccountStatuses, getInstanceRules, type InstanceRule } from '../api.ts'
import { errorMessage } from '@/lib/utils.ts'
import { Button } from '@/components/ui/button.tsx'
import { Checkbox } from '@/components/ui/checkbox.tsx'
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog.tsx'
import { Label } from '@/components/ui/label.tsx'
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'

type Category = mastodon.v1.ReportCategory

// Mastodon's report categories, worded as its report flow words them.
// `violation` is offered only when the server has rules to break, as upstream
// does: without them there is nothing to name.
const CATEGORIES: Record<Category, string> = {
  spam: 'Spam or scam',
  legal: 'Illegal content',
  violation: 'Breaks a server rule',
  other: 'Something else',
}

// Mastodon's Report::COMMENT_SIZE_LIMIT, which the server enforces too.
const COMMENT_LIMIT = 1000

// How many of the account's recent posts to offer as evidence.
const RECENT_POSTS = 20

function plainText(html: string): string {
  const doc = new DOMParser().parseFromString(html, 'text/html')
  return doc.body.textContent ?? ''
}

function domainOf(acct: string): string | null {
  return acct.includes('@') ? acct.split('@')[1] : null
}

export function ReportDialog({
  account,
  status,
  open,
  onOpenChange,
  token,
}: {
  account: mastodon.v1.Account
  // The post that prompted the report, if it started from one. It starts out
  // picked; a report is always against the account.
  status?: mastodon.v1.Status
  open: boolean
  onOpenChange: (open: boolean) => void
  token: string
}) {
  const [category, setCategory] = useState<Category>('other')
  const [rules, setRules] = useState<InstanceRule[]>([])
  const [ruleIds, setRuleIds] = useState<string[]>([])
  const [recent, setRecent] = useState<mastodon.v1.Status[]>([])
  const [statusIds, setStatusIds] = useState<string[]>([])
  const [comment, setComment] = useState('')
  const [forwardTo, setForwardTo] = useState<string[]>([])
  const [sending, setSending] = useState(false)

  // A fresh dialog each time it opens — a half-written report from the last
  // account is not a draft worth keeping.
  useEffect(() => {
    if (!open) return
    setCategory('other')
    setRuleIds([])
    setStatusIds(status ? [status.id] : [])
    setComment('')
    setForwardTo([])
    getInstanceRules().then(setRules).catch(() => setRules([]))
    getAccountStatuses(account.id, token)
      .then((list) => {
        const own = list.filter((s) => !s.reblog).slice(0, RECENT_POSTS)
        // The post the report started from stays on the list even when it is
        // older than the recent ones.
        setRecent(status && !own.some((s) => s.id === status.id) ? [status, ...own] : own)
      })
      .catch(() => setRecent(status ? [status] : []))
    // Keyed on the post's id: a parent re-rendering with a fresh copy of the
    // same post should not wipe what has been filled in.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, account.id, status?.id, token])

  // `acct` carries a domain only for remote accounts. Forwarding is offered to
  // that server, and to the servers of anyone the picked posts reply to — the
  // `forward_to_domains` Mastodon's report flow offers.
  const domain = domainOf(account.acct)
  const domains = domain
    ? [
        domain,
        ...new Set(
          recent
            .filter((s) => statusIds.includes(s.id) && s.inReplyToAccountId)
            .map((s) => s.mentions.find((m) => m.id === s.inReplyToAccountId)?.acct)
            .map((acct) => (acct ? domainOf(acct) : null))
            .filter((d): d is string => !!d && d !== domain),
        ),
      ]
    : []

  const categories: Record<string, string> =
    rules.length > 0
      ? CATEGORIES
      : { spam: CATEGORIES.spam, legal: CATEGORIES.legal, other: CATEGORIES.other }

  const toggle = (list: string[], id: string, on: boolean) =>
    on ? [...list, id] : list.filter((x) => x !== id)

  const needsRule = category === 'violation' && ruleIds.length === 0

  const submit = async () => {
    if (sending || needsRule) return
    setSending(true)
    const forwarding = forwardTo.filter((d) => domains.includes(d))
    try {
      await fileReport(token, {
        accountId: account.id,
        statusIds: statusIds.length > 0 ? statusIds : undefined,
        comment: comment.trim() || undefined,
        forward: domain ? forwarding.length > 0 : undefined,
        forwardToDomains:
          forwarding.filter((d) => d !== domain).length > 0 ? forwarding : undefined,
        category,
        ruleIds: category === 'violation' ? ruleIds : undefined,
      })
      toast.success(`Reported @${account.acct}. Moderators will take a look.`)
      onOpenChange(false)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setSending(false)
    }
  }

  // A dialog rather than an alertdialog: this is a form to fill in, not a
  // yes/no on something already decided. The block confirmation next door is
  // the other kind and stays an AlertDialog.
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Report @{account.acct}?</DialogTitle>
          <DialogDescription>
            Moderators on this server will see the report and the posts you pick.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <div className="space-y-1">
            <Label>Reason</Label>
            <Select
              items={categories}
              value={category}
              onValueChange={(v) => setCategory((v as Category) ?? 'other')}
            >
              <SelectTrigger className="w-full" aria-label="Reason">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  {Object.entries(categories).map(([key, label]) => (
                    <SelectItem key={key} value={key}>
                      {label}
                    </SelectItem>
                  ))}
                </SelectGroup>
              </SelectContent>
            </Select>
          </div>

          {category === 'violation' && (
            <fieldset className="space-y-1.5">
              <legend className="mb-1 text-sm font-medium">Which rules?</legend>
              {rules.map((rule) => (
                <Label key={rule.id} className="items-start text-sm leading-snug font-normal">
                  <Checkbox
                    checked={ruleIds.includes(rule.id)}
                    onCheckedChange={(on) => setRuleIds((ids) => toggle(ids, rule.id, on))}
                  />
                  <span>
                    {rule.text}
                    {rule.hint && (
                      <span className="text-muted-foreground block text-xs">{rule.hint}</span>
                    )}
                  </span>
                </Label>
              ))}
            </fieldset>
          )}

          {recent.length > 0 && (
            <fieldset className="space-y-1.5">
              <legend className="mb-1 text-sm font-medium">Posts to include (optional)</legend>
              <div className="max-h-56 space-y-1 overflow-y-auto rounded-lg border p-2">
                {recent.map((s) => (
                  <Label
                    key={s.id}
                    className="hover:bg-muted/50 items-start rounded p-1 text-sm leading-snug font-normal"
                  >
                    <Checkbox
                      checked={statusIds.includes(s.id)}
                      onCheckedChange={(on) => setStatusIds((ids) => toggle(ids, s.id, on))}
                    />
                    <span className="min-w-0">
                      <span className="line-clamp-2 break-words">
                        {s.spoilerText ? `CW: ${s.spoilerText}` : plainText(s.content) ||
                          (s.mediaAttachments.length > 0
                            ? `${s.mediaAttachments.length} attachment(s)`
                            : '(empty)')}
                      </span>
                      <span className="text-muted-foreground block text-xs">
                        {new Date(s.createdAt).toLocaleString()}
                      </span>
                    </span>
                  </Label>
                ))}
              </div>
            </fieldset>
          )}

          <div className="space-y-1">
            <Label htmlFor="report-comment">Anything else? (optional)</Label>
            <Textarea
              id="report-comment"
              value={comment}
              maxLength={COMMENT_LIMIT}
              onChange={(e) => setComment(e.target.value)}
              rows={3}
              className="resize-y"
              placeholder="What should a moderator know?"
            />
            <p className="text-muted-foreground text-xs">
              {comment.length}/{COMMENT_LIMIT}
            </p>
          </div>

          {domains.length > 0 && (
            <div className="space-y-1.5">
              <p className="text-muted-foreground text-xs">
                The account is from another server. Send an anonymized copy of the
                report there as well?
              </p>
              {domains.map((d) => (
                <Label key={d} className="text-sm font-normal">
                  <Switch
                    size="sm"
                    checked={forwardTo.includes(d)}
                    onCheckedChange={(on) => setForwardTo((list) => toggle(list, d, on))}
                  />
                  Forward to {d}
                </Label>
              ))}
            </div>
          )}
        </div>

        <DialogFooter>
          <DialogClose render={<Button variant="outline" disabled={sending} />}>
            Cancel
          </DialogClose>
          <Button variant="destructive" disabled={sending || needsRule} onClick={submit}>
            {sending ? 'Reporting…' : 'Report'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
