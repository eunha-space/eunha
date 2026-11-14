Invites
=======

Who may invite is Mastodon's `invite_users` permission, and it lives on the
**everyone role** — `user_roles` id -99, seeded by migration 009 with
`UserRole::Flags::DEFAULT`, which is that one permission. Every member has that
role unless given another, so an instance invites the way upstream does until it
says otherwise. One that would rather hand invites out itself takes *Invite
Users* off the base role on the [roles](./administration#roles) page.

Staff keep it through their own role. A role carrying `administrator` (1 << 0)
computes to every permission there is, and any other role's permissions are
unioned with the everyone role's — Mastodon's `UserRole#computed_permissions`,
which is why upstream's Admin role does not list `invite_users` and an admin can
still invite. `verify_credentials` reports that computed set rather than the raw
column, so a client shows the creation form for exactly the accounts the server
would allow.

Roles are edited there as in Mastodon, which lets the everyone role hold only
`Flags::SAFE`: `invite_users` and `invite_bypass_approval`.

The other half of `SAFE` is what an invite does to the approval queue. On an
instance whose sign-ups need approval, a self-created or legacy invite skips
review only when whoever wrote it holds `invite_bypass_approval`, which is
Mastodon's `Invite#bypass_approval?` — a question about the inviter, not about
whether an invite was used. The everyone role does not carry it, so an ordinary
member may bring someone and the admin still sees them first. An instance that
would rather an invite be the whole of the decision grants *Invite Users
without review* to the base role.

Staff-created invites bypass through the `administrator` flag. Staff-granted
invites also bypass review, independently of the receiving member’s role: the
grant is staff’s approval to admit those people. Email confirmation is still
required; expired, exhausted or unavailable invites cannot admit anyone.

Because a granted link admits without review, its last use is checked again
while the sign-up is saved, under a lock on the invite: two people signing up
with one single-use link at the same moment cannot both get in. The one who
loses is treated exactly as Mastodon treats a used-up code — refused on an
invite-only instance, queued for review on one that approves sign-ups. Codes
members make themselves are counted as Mastodon counts them.


Handing them out
----------------

That leaves an instance where only staff may invite, which on its own would
flatten the invite tree: every arrival would be a child of the admin rather than
of whoever actually brought them. So an admin can mint codes **into a member's
own account** instead — `POST /api/eunha/v1/invite_grants`, and the “Hand out
invites” form under **Moderation → Invites**, for one member or all eligible
local users at once. Granting to everyone has to be asked for with `all=true`;
a request that names nobody is refused rather than read as everybody. They
appear on that member's page to copy and pass on, and a signup through one
lands under them. The form creates one single-use link per person, accepts 1–25
people per member, defaults to seven days, and asks staff to review the
recipients, total links and expiry before granting. It has no member note or
approval selector. Bulk grants include confirmed, approved, functional local
users, staff included, at submission time; remote, disabled, suspended,
deleted, memorial and moved accounts are excluded, as are users whose role
requires two-factor authentication they have not set up. Future users need
another grant.

Repeated grants add links without replacing earlier ones. Each handout has its
own identity, grant date, staff sender and expiry. A transaction writes all
invite rows and their grant metadata together; no partial allowance is reported.
A code that happens to collide with an existing one is drawn again, as
Mastodon's `Invite#set_code` does.

**Handouts**, on the same page, lists every grant newest first
(`GET /api/eunha/v1/invite_grants`): who gave it, to whom, how many links were
used and are still available. **Revoke**
(`POST /api/eunha/v1/invite_grants/:id/revoke`) expires the grant's links that
can still be used, the way revoking one invite does, and records who revoked it.
Rows, use counts and the tree stay. This list is the record of staff admitting
people through other members; it is not written to Mastodon's audit log, whose
page could not show an entry about a target Mastodon has no class for.

The count is the limit; there is no allowance to keep books on, because the
codes themselves are the allowance. `manage_invites` is what it takes to hand
them out, and listing your own invites takes no permission at all — a member who
cannot create one still has to be able to read what they were given, which is
where eunha parts company with `InvitesController#index`.


Every invite
------------

A role with `manage_invites` sees every invite on the server at
`/api/v1/admin/invites`, newest first and forty a page, each with who made it,
how often it was used and whether it can still be used; `?available=1` and
`?expired=1` narrow it as Mastodon's filter does.
`POST /api/v1/admin/invites/deactivate_all` expires every invite that can
still be used, and such a role may expire any one invite with
`DELETE /api/v1/invites/:id`. This is Mastodon's `Admin::InvitesController`,
which logs nothing, and the web client's *Invites* page under `/admin`; new
invites are still made on the invite page.


Joining through an invite
-------------------------

The signup page checks the code before submission and explains whether it is
expired, fully used, or unavailable. A valid code identifies its inviter and
explains automatic following when enabled. Approval guidance and the reason
field follow the inviter's current bypass permission, not merely the presence
of a code, except that staff-granted links always bypass review.
`/api/eunha/v1/invite?invite=CODE` serves this public resolution;
registration checks the code again when submitted.

Links are Mastodon's: `https://DOMAIN/invite/CODE`, which opens the sign-up page
with the code filled in. Links Mastodon handed out keep working after a move to
eunha, and links handed out here keep working after a move back. The same path
asked for as JSON (an `Accept: application/json` header, or `/invite/CODE.json`)
answers as Mastodon's `Api::V1::InvitesController#show` does for the apps: the
code and the instance's API URL, 404 for an unknown code, 401 for one no longer
good for use, 403 when sign-ups from the address are blocked. Older
`/signup?invite=CODE` and `/auth/signup?invite=CODE` links continue to work, and
pasting a whole link into the invite code field keeps only the code.


Managing links
--------------

Personal and admin lists distinguish available, fully used, expired and
unavailable codes using the server's validity checks. Who counts as able to use
an invite (`User#functional?`) is defined once, in the `eunha.user_standings`
view, which the inviter check, the grant recipients and the admin list share.

Each personal link also says who joined through it: members by name, members who
hide their network only as a count, and sign-ups still awaiting confirmation or
approval as a separate count. Mastodon counts a use when the account is created
and never gives it back, so a sign-up that is never confirmed (and is cleaned up
after a week) still used the link; the count shows why. Expiring a personal link
retains its row, note and usage history. Only usable links offer Copy and Expire
(or Revoke); admin lists expose the complete signup URL as well.

Handing out codes requires choosing a recipient explicitly. The member picker
supports username search, reports loading failures with a retry, and summarizes
how many links and admissions will be created before submission. Granting in a
member’s name preserves their place in the tree; the staff grant itself
authorizes admission without changing the member’s permissions.


Exploring the tree
------------------

The invite tree is available to signed-in members. It starts with **My branch**,
showing the member's ancestors and invitees (`?scope=branch`, served as just
that); **Whole instance** shows the full forest. **Find me** reveals and focuses
the signed-in member. The view, the search and the member in focus are kept in
the page's address, so a link can point at one member's place. **Collapse all**
folds the view. Searching usernames and display names keeps matching members'
ancestors visible and searches the selected view. The tree follows the WAI-ARIA
tree pattern: arrow keys move between members and open or close branches, Home
and End jump to the ends, and Enter opens a member's profile.

Each member shows their direct invitees, everyone below them in all, and their
join date. Pending, unconfirmed, suspended and deleted members are not listed.
An inviter who is not a member any more — deleted, suspended, or otherwise gone
— is shown as **A former member**, one anonymous place their invitees stay
under, rather than splitting the branch into roots. A member who hides their
network (Mastodon's *hide your follows and followers* setting) is shown as **A
member who hides their network**, without name, picture or date, to everyone
but themselves and staff with `manage_invites`; their branch stays in place. A
root is labeled **No inviter recorded** or, for cyclic lineage from damaged or
imported data, **Earlier lineage is unavailable**; cycles are broken into
visible branches so no member silently disappears.

The lineage `eunha.invite_lineage` keeps across a deletion is written by eunha's
account deletion. A deletion done while the database is run by Mastodon removes
the link for good, as Mastodon itself would.

The server sends the tree as a flat list in which parents precede their
children, with an `ETag`, so neither end walks a long chain recursively and an
unchanged tree is answered with 304. Staff with `manage_invites` can open a
preselected grant form from a member's row; both pages link to each other.


Finding your invites
--------------------

Signed-in members have **Invite people** beside **Local** in the desktop sidebar
and mobile navigation drawer when they have a usable invite link. Moderators
and admins with access to Moderation always see the entry, including when no
links are usable or availability cannot be loaded. For regular members the
entry is hidden while availability is unknown or no links can be used, even
when the member may create links; `/invites` remains directly accessible.
Multiple-use and unlimited-use links still make the entry visible. A quiet
count means **available single-use invite links**. It appears only when every
available link admits one person, hides at zero or when any usable link has
multiple or unlimited uses, and is distinct from the unread-notifications
badge. It refreshes on navigation, focus, invite changes and once a minute;
local expiry also removes links from the count. Navigation and failed refreshes
retain the last successfully loaded availability for the signed-in account,
so the entry does not disappear while checking again.

**Your invites** leads with the available links and a Copy invite link action.
When all available links are single-use it says how many people they admit;
otherwise it reports links and the selected link’s remaining uses. Members who
may create links also see Create invite link, so a grant is not presented as a
total quota on their account. Copying leaves uses unchanged. Choose another link
prefers one not copied during the current visit. The default selection expires
soonest; an explicit selection stays selected as new grants arrive, until it
becomes unavailable.

The sharing panel shows expiry and admission details for the selected link.
**Manage links** groups Available,
Fully used, Expired and Unavailable links by grant, retaining history. The
invite tree remains a related action. Staff open grants from Moderation or a
member’s tree row.


Database compatibility
----------------------

Migration 028 adds only `eunha.invite_grants` and `eunha.granted_invites`;
migration 041 adds the grant's `revoked_at` and `revoked_by_account_id` and the
`eunha.user_standings` view. Mastodon’s `public.invites`, `users.invite_id` and
role tables remain unchanged: revoking a grant only sets `expires_at`, as
Mastodon's own `Expireable#expire!` does.
No previous invites are backfilled: existing rows cannot reliably distinguish
staff grants from self-created codes, so legacy invites retain role-based
review.

Switching back to Mastodon preserves codes, ownership, usage, expiry and
lineage. Mastodon ignores eunha’s grant metadata and determines approval from
the inviter’s current role again. The public schema stays compatible; the
automatic approval of staff-granted codes is a deliberate behavioral divergence.
