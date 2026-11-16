import type { ReactNode } from 'react'

import { cn } from '@/lib/utils'
import { Skeleton } from '@/components/ui/skeleton.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'
import { t } from '@/i18n.tsx'

/*
 * Placeholders shaped like the content they stand in for, built from shadcn's
 * `Skeleton`. Each wrapper is announced once as a busy status, so a screen
 * reader hears "Loading" rather than a run of empty boxes.
 */

function Loading({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <div role="status" aria-label={t({ id: 'combobox.loading', defaultMessage: 'Loading' })} aria-busy="true" className={className}>
      {children}
    </div>
  )
}

/** Widths cycled through so a run of placeholder lines doesn't look ruled. */
const WIDTHS = ['w-full', 'w-11/12', 'w-4/5', 'w-2/3', 'w-3/4']

function lines(n: number, className = 'h-4') {
  return Array.from({ length: n }, (_, i) => (
    <Skeleton key={i} className={cn(className, i === n - 1 && n > 1 ? 'w-3/5' : WIDTHS[i % WIDTHS.length])} />
  ))
}

/** One post: avatar, name and handle, two lines of text, the action row. */
export function StatusSkeleton() {
  return (
    <div className="space-y-2 px-3 py-3 sm:px-4">
      <div className="flex items-center gap-2">
        <Skeleton className="size-8 shrink-0 rounded-full" />
        <Skeleton className="h-4 w-28" />
        <Skeleton className="h-4 w-20" />
        <Skeleton className="ml-auto h-3 w-8" />
      </div>
      <Skeleton className="h-4 w-full" />
      <Skeleton className="h-4 w-3/4" />
      <div className="flex gap-6 pt-1">
        <Skeleton className="h-4 w-8" />
        <Skeleton className="h-4 w-8" />
        <Skeleton className="h-4 w-8" />
      </div>
    </div>
  )
}

/** A timeline of posts, or the next page of one. */
export function FeedSkeleton({ count = 3 }: { count?: number }) {
  return (
    <Loading>
      <TimelineStack>
        {Array.from({ length: count }, (_, i) => (
          <StatusSkeleton key={i} />
        ))}
      </TimelineStack>
    </Loading>
  )
}

/** One `AccountRow`: avatar, name, handle. */
export function AccountRowSkeleton() {
  return (
    <div className="flex items-center gap-3 p-2">
      <Skeleton className="size-10 shrink-0 rounded-full" />
      <div className="min-w-0 flex-1 space-y-1.5">
        <Skeleton className="h-4 w-32" />
        <Skeleton className="h-3.5 w-24" />
      </div>
    </div>
  )
}

/** A list of accounts. */
export function AccountListSkeleton({ count = 4 }: { count?: number }) {
  return (
    <Loading className="space-y-1">
      {Array.from({ length: count }, (_, i) => (
        <AccountRowSkeleton key={i} />
      ))}
    </Loading>
  )
}

/** A notification: a line naming what happened, then what it was about. */
export function NotificationSkeleton() {
  return (
    <div className="space-y-2 px-3 py-3 sm:px-4">
      <div className="flex items-center gap-2">
        <Skeleton className="size-4 shrink-0 rounded-full" />
        <Skeleton className="size-8 shrink-0 rounded-full" />
        <Skeleton className="h-4 w-40" />
      </div>
      <Skeleton className="h-4 w-full" />
      <Skeleton className="h-4 w-2/3" />
    </div>
  )
}

export function NotificationsSkeleton({ count = 4 }: { count?: number }) {
  return (
    <Loading>
      <TimelineStack>
        {Array.from({ length: count }, (_, i) => (
          <NotificationSkeleton key={i} />
        ))}
      </TimelineStack>
    </Loading>
  )
}

/**
 * Rows of a plain list — lists, filters, tags, blocked domains, admin
 * entries: a title, a line under it, and room for a control at the end.
 */
export function RowsSkeleton({
  count = 4,
  action = false,
  className,
}: {
  count?: number
  action?: boolean
  className?: string
}) {
  return (
    <Loading className={cn('space-y-2', className)}>
      {Array.from({ length: count }, (_, i) => (
        <div key={i} className="flex items-center gap-3 rounded-lg border p-3">
          <div className="min-w-0 flex-1 space-y-1.5">
            <Skeleton className={cn('h-4', ['w-40', 'w-56', 'w-32', 'w-48'][i % 4])} />
            <Skeleton className={cn('h-3.5', ['w-64', 'w-44', 'w-56', 'w-36'][i % 4])} />
          </div>
          {action && <Skeleton className="h-8 w-20 shrink-0" />}
        </div>
      ))}
    </Loading>
  )
}

/** A table: a header row and some body rows. */
export function TableSkeleton({ rows = 5, columns = 4 }: { rows?: number; columns?: number }) {
  return (
    <Loading className="space-y-3">
      <div className="flex gap-4 border-b pb-2">
        {Array.from({ length: columns }, (_, c) => (
          <Skeleton key={c} className="h-4 flex-1" />
        ))}
      </div>
      {Array.from({ length: rows }, (_, r) => (
        <div key={r} className="flex gap-4">
          {Array.from({ length: columns }, (_, c) => (
            <Skeleton key={c} className={cn('h-5 flex-1', c === 0 && 'flex-[2]')} />
          ))}
        </div>
      ))}
    </Loading>
  )
}

/** Prose: a heading and a few paragraphs, for policies and long text. */
export function TextSkeleton({ paragraphs = 3, heading = true }: { paragraphs?: number; heading?: boolean }) {
  return (
    <Loading className="space-y-5">
      {heading && <Skeleton className="h-6 w-1/3" />}
      {Array.from({ length: paragraphs }, (_, i) => (
        <div key={i} className="space-y-2">
          {lines(3)}
        </div>
      ))}
    </Loading>
  )
}

/** A few lines of text, inline in a panel or a dialog. */
export function LinesSkeleton({ count = 3, className }: { count?: number; className?: string }) {
  return <Loading className={cn('space-y-2', className)}>{lines(count)}</Loading>
}

/** A form: labelled fields and a submit button. */
export function FormSkeleton({ fields = 4 }: { fields?: number }) {
  return (
    <Loading className="space-y-5">
      {Array.from({ length: fields }, (_, i) => (
        <div key={i} className="space-y-2">
          <Skeleton className="h-4 w-32" />
          <Skeleton className="h-9 w-full" />
        </div>
      ))}
      <Skeleton className="h-9 w-24" />
    </Loading>
  )
}

/** Cards in a grid: admin dashboards, account details. */
export function CardsSkeleton({ count = 4, className }: { count?: number; className?: string }) {
  return (
    <Loading className={cn('grid gap-3 sm:grid-cols-2', className)}>
      {Array.from({ length: count }, (_, i) => (
        <div key={i} className="space-y-2 rounded-xl border p-4">
          <Skeleton className="h-4 w-24" />
          <Skeleton className="h-7 w-16" />
        </div>
      ))}
    </Loading>
  )
}

/** A detail page: a heading, a block of facts, then a body. */
export function DetailSkeleton() {
  return (
    <Loading className="space-y-6">
      <div className="flex items-center gap-3">
        <Skeleton className="size-12 shrink-0 rounded-full" />
        <div className="flex-1 space-y-2">
          <Skeleton className="h-5 w-48" />
          <Skeleton className="h-4 w-32" />
        </div>
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        {Array.from({ length: 4 }, (_, i) => (
          <div key={i} className="space-y-1.5">
            <Skeleton className="h-3.5 w-20" />
            <Skeleton className="h-4 w-36" />
          </div>
        ))}
      </div>
      <div className="space-y-2">{lines(4)}</div>
    </Loading>
  )
}

/** A profile: header image, avatar, names, bio, counts, then posts. */
export function ProfileSkeleton() {
  return (
    <Loading className="space-y-4">
      <Skeleton className="aspect-[3/1] w-full rounded-xl" />
      <div className="-mt-12 flex items-end gap-3 px-4">
        <Skeleton className="size-20 shrink-0 rounded-full border-4 border-background" />
        <Skeleton className="ml-auto h-9 w-24" />
      </div>
      <div className="space-y-2 px-4">
        <Skeleton className="h-6 w-48" />
        <Skeleton className="h-4 w-36" />
      </div>
      <div className="space-y-2 px-4">{lines(3)}</div>
      <div className="flex gap-6 px-4">
        <Skeleton className="h-4 w-20" />
        <Skeleton className="h-4 w-20" />
        <Skeleton className="h-4 w-20" />
      </div>
      <TimelineStack>
        <StatusSkeleton />
        <StatusSkeleton />
      </TimelineStack>
    </Loading>
  )
}

/** Hashtags: a name, a line of usage, and a small chart's room. */
export function TagsSkeleton({ count = 5 }: { count?: number }) {
  return (
    <Loading className="space-y-3">
      {Array.from({ length: count }, (_, i) => (
        <div key={i} className="flex items-center gap-3">
          <div className="flex-1 space-y-1.5">
            <Skeleton className={cn('h-4', ['w-28', 'w-36', 'w-24'][i % 3])} />
            <Skeleton className="h-3.5 w-40" />
          </div>
          <Skeleton className="h-6 w-16" />
        </div>
      ))}
    </Loading>
  )
}

/** A grid of square tiles, such as custom emoji. */
export function GridSkeleton({ count = 24, className }: { count?: number; className?: string }) {
  return (
    <Loading className={cn('grid grid-cols-8 gap-2', className)}>
      {Array.from({ length: count }, (_, i) => (
        <Skeleton key={i} className="aspect-square w-full" />
      ))}
    </Loading>
  )
}

/** A settings section: a bordered panel with a heading and a control or two. */
export function SectionSkeleton({ rows = 2 }: { rows?: number }) {
  return (
    <Loading className="space-y-3 rounded-lg border p-4">
      <Skeleton className="h-5 w-40" />
      {Array.from({ length: rows }, (_, i) => (
        <Skeleton key={i} className={cn('h-9', i === 0 ? 'w-full' : 'w-2/3')} />
      ))}
    </Loading>
  )
}
