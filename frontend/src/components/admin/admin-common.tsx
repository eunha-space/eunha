import { useState, type ReactNode } from 'react'
import { Link } from 'react-router-dom'
import { toast } from 'sonner'

import {
  publicAccount,
  type Account,
  type AdminAccount,
  type Status,
} from '../../admin-api.ts'
import { errorMessage } from '@/lib/utils.ts'
import { RelativeTime } from '@/components/relative-time.tsx'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog.tsx'
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select.tsx'

export function formatDate(value: string | null | undefined): string {
  if (!value) return ''
  const date = new Date(value)
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString()
}

/**
 * An account in a moderation list: who it is, linking to its moderation page
 * rather than its profile, since that is where a moderator goes next.
 */
export function AdminAccountLink({
  account,
  size = 'default',
}: {
  account: AdminAccount | Account
  size?: 'default' | 'sm'
}) {
  const pub = publicAccount(account)
  const name = pub.display_name || pub.username
  return (
    <Link
      to={`/admin/accounts/${pub.id}`}
      className="flex min-w-0 items-center gap-2 no-underline hover:underline"
    >
      <Avatar className={size === 'sm' ? 'size-6' : 'size-9'}>
        <AvatarImage src={pub.avatar} alt="" />
        <AvatarFallback>{name.slice(0, 1).toUpperCase()}</AvatarFallback>
      </Avatar>
      <span className="min-w-0">
        <span className="block truncate text-sm font-medium">{name}</span>
        {size === 'default' && (
          <span className="text-muted-foreground block truncate text-xs">@{pub.acct}</span>
        )}
      </span>
    </Link>
  )
}

/** What has been done to an account, as Mastodon's account list labels it. */
export function AccountStateBadges({ account }: { account: AdminAccount }) {
  const badges: { label: string; destructive?: boolean }[] = []
  if (account.suspended) badges.push({ label: 'Suspended', destructive: true })
  if (account.disabled) badges.push({ label: 'Frozen', destructive: true })
  if (account.silenced) badges.push({ label: 'Limited', destructive: true })
  if (account.sensitized) badges.push({ label: 'Sensitive' })
  if (account.domain === null && !account.approved) badges.push({ label: 'Pending' })
  if (account.domain === null && !account.confirmed) badges.push({ label: 'Unconfirmed' })
  if (account.role && account.role.name && account.role.id !== '-99') {
    badges.push({ label: account.role.name })
  }
  if (badges.length === 0) return null
  return (
    <span className="flex flex-wrap gap-1">
      {badges.map((b) => (
        <Badge key={b.label} variant={b.destructive ? 'destructive' : 'secondary'}>
          {b.label}
        </Badge>
      ))}
    </span>
  )
}

/**
 * A button that asks before it acts. For the moderation calls that cannot be
 * taken back — deleting an account, lifting a block — and the toast says what
 * happened either way.
 */
export function ConfirmButton({
  title,
  description,
  confirmLabel,
  onConfirm,
  children,
  variant = 'outline',
  size = 'sm',
  destructive = true,
  disabled,
}: {
  title: string
  description: ReactNode
  confirmLabel: string
  onConfirm: () => Promise<void>
  children: ReactNode
  variant?: React.ComponentProps<typeof Button>['variant']
  size?: React.ComponentProps<typeof Button>['size']
  destructive?: boolean
  disabled?: boolean
}) {
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const run = async () => {
    setBusy(true)
    try {
      await onConfirm()
      setOpen(false)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }
  return (
    <>
      <Button variant={variant} size={size} disabled={disabled} onClick={() => setOpen(true)}>
        {children}
      </Button>
      <AlertDialog open={open} onOpenChange={setOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{title}</AlertDialogTitle>
            <AlertDialogDescription>{description}</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={busy}>Cancel</AlertDialogCancel>
            <AlertDialogAction
              variant={destructive ? 'destructive' : 'default'}
              disabled={busy}
              onClick={() => void run()}
            >
              {confirmLabel}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  )
}

/**
 * A post as a moderator reads it: whose, when, what it says behind any
 * content warning, and what it carries. No buttons to favourite or boost — a
 * report is not the place to interact with what was reported.
 */
export function AdminStatus({ status }: { status: Status }) {
  const s = status.reblog ?? status
  return (
    <article className="space-y-1.5 rounded-lg border p-3">
      <div className="flex items-center gap-2 text-xs">
        <AdminAccountLink account={s.account} size="sm" />
        <span className="text-muted-foreground ml-auto shrink-0">
          {s.visibility} ·{' '}
          {s.url ? (
            <a href={s.url} target="_blank" rel="noreferrer">
              <RelativeTime value={s.created_at} />
            </a>
          ) : (
            <RelativeTime value={s.created_at} />
          )}
        </span>
      </div>
      {s.spoiler_text && (
        <p className="text-sm font-medium">CW: {s.spoiler_text}</p>
      )}
      <div
        className="text-sm break-words [&_a]:font-medium [&_a]:text-primary [&_a]:underline"
        dangerouslySetInnerHTML={{ __html: s.content }}
      />
      {s.media_attachments.length > 0 && (
        <div className="flex flex-wrap gap-2">
          {s.media_attachments.map((m) =>
            m.preview_url ? (
              <a key={m.id} href={m.url ?? m.preview_url} target="_blank" rel="noreferrer">
                <img
                  src={m.preview_url}
                  alt={m.description ?? ''}
                  className={`size-20 rounded object-cover ${s.sensitive ? 'blur-sm hover:blur-none' : ''}`}
                />
              </a>
            ) : (
              <Badge key={m.id} variant="outline">
                {m.type}
              </Badge>
            ),
          )}
        </div>
      )}
    </article>
  )
}

/**
 * A select over a fixed set of labelled values — the shape every filter and
 * severity picker on these pages has. Base UI's Select takes `items` as value
 * → label for what the trigger shows, as on the invites page.
 */
export function ChoiceSelect<T extends string>({
  label,
  value,
  items,
  onChange,
  className,
}: {
  label: string
  value: T
  items: Record<T, string>
  onChange: (value: T) => void
  className?: string
}) {
  return (
    <Select items={items} value={value} onValueChange={(v) => v && onChange(v as T)}>
      <SelectTrigger className={className ?? 'w-full'} aria-label={label}>
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        <SelectGroup>
          {(Object.keys(items) as T[]).map((key) => (
            <SelectItem key={key} value={key}>
              {items[key]}
            </SelectItem>
          ))}
        </SelectGroup>
      </SelectContent>
    </Select>
  )
}
