import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { Code, FileText, Network, Shield, Ticket } from 'lucide-react'

import { getInstance } from '../api.ts'
import { getToken } from '../auth.ts'
import type { mastodon } from '../masto.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { AccountRow } from '@/components/account-row.tsx'

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="space-y-1">
      <h2 className="text-muted-foreground text-sm font-semibold">{title}</h2>
      {children}
    </section>
  )
}

// What an instance says about itself when asked. Every field here is optional
// on the wire — an instance that has configured none of them still renders a
// page, rather than a stack of empty headings.
export default function About() {
  const [instance, setInstance] = useState<mastodon.v2.Instance | null>(null)
  const token = getToken()

  useEffect(() => {
    getInstance().then(setInstance).catch(() => {})
  }, [])

  if (!instance) {
    return (
      <div className="page-frame">
        <TopBar />
        <p className="text-muted-foreground text-sm">Loading…</p>
      </div>
    )
  }

  const { registrations, contact, languages, usage, rules, sourceUrl } = instance
  const activeMonth = usage?.users?.activeMonth
  // Mastodon 4.4 added `urls.terms_of_service`; masto's types predate it.
  const urls = instance.configuration?.urls as { termsOfService?: string | null } | undefined
  const termsUrl = urls?.termsOfService
  // `enabled` is the instance's own switch; approval is a second gate behind it.
  const signup = !registrations?.enabled
    ? 'Closed — new accounts are by invitation only.'
    : registrations.approvalRequired
      ? 'Open, and each new account is reviewed before it can sign in.'
      : 'Open to anyone.'

  return (
    <div className="page-frame">
      <TopBar title={instance.title} />
      <div className="space-y-5">
        <section className="space-y-2">
          <h1 className="text-2xl font-bold">{instance.title}</h1>
          <p className="text-foreground/90">{instance.description}</p>
          <p className="text-muted-foreground text-sm">
            {instance.domain} · running eunha {__COMMIT_HASH__} ·{' '}
            {instance.version}
          </p>
        </section>

        <Section title="Signing up">
          <p className="text-sm">{signup}</p>
        </Section>

        {contact?.account && (
          <Section title="Run by">
            <AccountRow account={contact.account} />
            {contact.email && (
              <a
                href={`mailto:${contact.email}`}
                className="text-primary inline-block px-2 text-sm"
              >
                {contact.email}
              </a>
            )}
          </Section>
        )}

        {!!languages?.length && (
          <Section title="Languages">
            <p className="text-sm">{languages.join(', ')}</p>
          </Section>
        )}

        {typeof activeMonth === 'number' && (
          <Section title="People here">
            <p className="text-sm">
              {activeMonth} {activeMonth === 1 ? 'person has' : 'people have'}{' '}
              posted in the last month.
            </p>
          </Section>
        )}

        {/* eunha serves no rules today — `get_instance_rules` returns an empty
            list — so this is here for an instance that grows some, and stays
            invisible until then. */}
        {!!rules?.length && (
          <Section title="Rules">
            <ol className="list-decimal space-y-1 pl-5 text-sm">
              {rules.map((rule) => (
                <li key={rule.id}>{rule.text}</li>
              ))}
            </ol>
          </Section>
        )}

        {/* Mastodon's about page links its policies rather than inlining them.
            There is always a privacy policy — the server falls back to
            Mastodon's own — while terms of service exist only once an
            administrator has published some, which `urls.terms_of_service`
            says. */}
        <Section title="Policies">
          <div className="flex flex-col gap-1">
            <Link
              to="/privacy-policy"
              className="text-primary inline-flex items-center gap-2 text-sm font-medium"
            >
              <Shield className="size-4" /> Privacy policy
            </Link>
            {termsUrl && (
              <Link
                to="/terms-of-service"
                className="text-primary inline-flex items-center gap-2 text-sm font-medium"
              >
                <FileText className="size-4" /> Terms of service
              </Link>
            )}
          </div>
        </Section>

        <Section title="This software">
          <a
            href={sourceUrl}
            target="_blank"
            rel="noopener noreferrer"
            className="text-primary inline-flex items-center gap-2 text-sm font-medium"
          >
            <Code className="size-4" /> Source code
          </a>
        </Section>

        {token && (
          <Section title="Invites">
            <div className="flex flex-col gap-1">
              {/* Shown to every member: not everyone may create an invite, but
                  anyone can be handed one, and this is where they read it. */}
              <Link
                to="/invites"
                className="text-primary inline-flex items-center gap-2 text-sm font-medium"
              >
                <Ticket className="size-4" /> Your invites
              </Link>
              <Link
                to="/invite-tree"
                className="text-primary inline-flex items-center gap-2 text-sm font-medium"
              >
                <Network className="size-4" /> View the invite tree
              </Link>
            </div>
          </Section>
        )}
      </div>
    </div>
  )
}
