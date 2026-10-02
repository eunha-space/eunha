import { Fragment, type ReactNode } from 'react'
import { Link } from 'react-router-dom'

import { type ActionLog } from '../../admin-api.ts'
import { formatDate } from '@/components/admin/admin-common.tsx'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'

/** Where an entry's target links: a page here, or a post's own address. */
function TargetLink({ target }: { target: NonNullable<ActionLog['target']> }) {
  if (!target.href) return <span className="font-medium">{target.text}</span>
  if (target.href.startsWith('/')) {
    return (
      <Link to={target.href} className="font-medium">
        {target.text}
      </Link>
    )
  }
  return (
    <a href={target.href} target="_blank" rel="noreferrer" className="font-medium">
      {target.text}
    </a>
  )
}

/**
 * One audit log entry, as Mastodon's `_action_log` partial renders it: who
 * acted, the sentence its locale gives the action, with the target linked,
 * and when.
 */
export function ActionLogEntry({ log }: { log: ActionLog }) {
  const name = log.account?.username ?? ''
  const parts: ReactNode[] = log.template.split(/(%\{name\}|%\{target\})/).map((part, i) => {
    if (part === '%{name}') {
      return (
        <span key={i} className="font-medium">
          {name}
        </span>
      )
    }
    if (part === '%{target}') {
      return log.target ? <TargetLink key={i} target={log.target} /> : <Fragment key={i} />
    }
    return <Fragment key={i}>{part}</Fragment>
  })
  return (
    <div className="flex items-start gap-3 p-2.5">
      <Avatar className="size-8">
        <AvatarImage src={log.account?.avatar} alt="" />
        <AvatarFallback>{name.slice(0, 1).toUpperCase()}</AvatarFallback>
      </Avatar>
      <div className="min-w-0 flex-1">
        <p className="text-sm break-words">
          {parts}
          {log.changes && <> {log.changes}</>}
        </p>
        <time className="text-muted-foreground text-xs" dateTime={log.created_at}>
          {formatDate(log.created_at)}
        </time>
      </div>
    </div>
  )
}
