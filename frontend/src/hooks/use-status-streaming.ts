import { useCallback } from 'react'

import type { mastodon } from '../masto.ts'
import { useStreamingSubscription } from './use-streaming-subscription.ts'

type StatusFeed = {
  mutate: (fn: (items: mastodon.v1.Status[]) => mastodon.v1.Status[]) => void
}

function hasStatusId(status: mastodon.v1.Status, id: string) {
  return status.id === id || status.reblog?.id === id
}

function mergeStatus(status: mastodon.v1.Status, updated: mastodon.v1.Status) {
  if (status.reblog?.id === updated.id) {
    return { ...status, reblog: keepQuoteApproval(updated, status.reblog) }
  }
  if (status.id === updated.id) return keepQuoteApproval(updated, status)
  return status
}

// A status that arrives without a quote policy keeps the one it had.
function keepQuoteApproval(updated: mastodon.v1.Status, old: mastodon.v1.Status) {
  return updated.quoteApproval ? updated : { ...updated, quoteApproval: old.quoteApproval }
}

// Mastodon's `bogusQuotePolicy`: the public and hashtag streams send one
// payload to everyone, written for no one, so its `quote_approval` says
// `denied` to every reader. Like Mastodon's web client, drop it rather than
// believe it, down to a boost's post.
function withoutQuoteApproval(status: mastodon.v1.Status): mastodon.v1.Status {
  const bare = { ...status, quoteApproval: null } as unknown as mastodon.v1.Status
  if (status.reblog) bare.reblog = withoutQuoteApproval(status.reblog)
  return bare
}

export function useStatusStreaming({
  enabled = true,
  token,
  subscribe,
  feed,
  bogusQuotePolicy = false,
}: {
  enabled?: boolean
  token?: string
  // The stream is a public or hashtag one; see `withoutQuoteApproval`.
  bogusQuotePolicy?: boolean
  subscribe: (client: mastodon.streaming.Client) => mastodon.streaming.Subscription
  feed: StatusFeed
}) {
  const { mutate } = feed
  const onEvent = useCallback(
    (event: mastodon.streaming.Event) => {
      if (
        bogusQuotePolicy &&
        (event.event === 'update' || event.event === 'status.update')
      ) {
        event = { ...event, payload: withoutQuoteApproval(event.payload) }
      }
      if (event.event === 'update') {
        // A message arriving live would otherwise appear at the top of the
        // home timeline, which is the one place Messages exists to keep it
        // out of. Dropping it here rather than in the reducer keeps the feed's
        // pagination cursor on what the server sent.
        if ((event.payload.reblog ?? event.payload).visibility === 'direct') {
          return
        }
        mutate((items) =>
          items.some((item) => hasStatusId(item, event.payload.id))
            ? items.map((item) => mergeStatus(item, event.payload))
            : [event.payload, ...items],
        )
        return
      }

      if (event.event === 'status.update') {
        mutate((items) =>
          items.map((item) => mergeStatus(item, event.payload)),
        )
        return
      }

      if (event.event === 'delete') {
        mutate((items) =>
          items.filter((item) => !hasStatusId(item, event.payload)),
        )
      }
    },
    [mutate, bogusQuotePolicy],
  )

  useStreamingSubscription({ enabled, token, subscribe, onEvent })
}
