import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { createBrowserRouter, Navigate, RouterProvider } from 'react-router-dom'
import Home from './pages/Home.tsx'
import Callback from './pages/Callback.tsx'
import Profile from './pages/Profile.tsx'
import StatusThread from './pages/StatusThread.tsx'
import PublicTimeline from './pages/PublicTimeline.tsx'
import Notifications from './pages/Notifications.tsx'
import FollowRequests from './pages/FollowRequests.tsx'
import SearchPage from './pages/Search.tsx'
import TagTimeline from './pages/TagTimeline.tsx'
import AccountList from './pages/AccountList.tsx'
import StatusReactions from './pages/StatusReactions.tsx'
import Bookmarks from './pages/Bookmarks.tsx'
import Messages from './pages/Messages.tsx'
import Explore from './pages/Explore.tsx'
import StatusHistory from './pages/StatusHistory.tsx'
import BlockedAccounts from './pages/BlockedAccounts.tsx'
import About from './pages/About.tsx'
import TermsOfService from './pages/TermsOfService.tsx'
import PrivacyPolicy from './pages/PrivacyPolicy.tsx'
import InviteTree from './pages/InviteTree.tsx'
import Invites from './pages/Invites.tsx'
import Signup from './pages/Signup.tsx'
import Settings from './pages/Settings.tsx'
import ImportExport from './pages/ImportExport.tsx'
import AdminIndex from './pages/admin/AdminIndex.tsx'
import AdminDashboard from './pages/admin/Dashboard.tsx'
import AdminReports from './pages/admin/Reports.tsx'
import AdminReportDetail from './pages/admin/ReportDetail.tsx'
import AdminAccounts from './pages/admin/Accounts.tsx'
import AdminAccountDetail from './pages/admin/AccountDetail.tsx'
import AdminDomainBlocks from './pages/admin/DomainBlocks.tsx'
import AdminDomainAllows from './pages/admin/DomainAllows.tsx'
import AdminIpBlocks from './pages/admin/IpBlocks.tsx'
import AdminEmailDomainBlocks from './pages/admin/EmailDomainBlocks.tsx'
import AdminCanonicalEmailBlocks from './pages/admin/CanonicalEmailBlocks.tsx'
import AdminTrends from './pages/admin/Trends.tsx'
import AdminTags from './pages/admin/Tags.tsx'
import AdminCustomEmojis from './pages/admin/CustomEmojis.tsx'
import AdminEmailSubscriptions from './pages/admin/EmailSubscriptions.tsx'
import AdminEmailSubscriptionAccount from './pages/admin/EmailSubscriptionAccount.tsx'
import {
  AdminTermsOfServiceDraft,
  AdminTermsOfServiceGenerate,
  AdminTermsOfServiceHistory,
  AdminTermsOfServiceIndex,
  AdminTermsOfServicePreview,
} from './pages/admin/TermsOfService.tsx'
import { TermsOfServiceInterstitial } from './components/terms-of-service-interstitial.tsx'
import AdminActionLogs from './pages/admin/ActionLogs.tsx'
import AdminAppeals from './pages/admin/Appeals.tsx'
import AdminWarningPresets from './pages/admin/WarningPresets.tsx'
import AdminUsernameBlocks from './pages/admin/UsernameBlocks.tsx'
import AdminAccountStatuses from './pages/admin/AccountStatuses.tsx'
import AdminAccountStatusDetail from './pages/admin/AccountStatusDetail.tsx'
import AdminRelationships from './pages/admin/Relationships.tsx'
// The server administration Mastodon has only as server-rendered admin pages.
import AdminSettings from './pages/admin/Settings.tsx'
import AdminRules from './pages/admin/Rules.tsx'
import AdminRoles from './pages/admin/Roles.tsx'
import AdminAnnouncements from './pages/admin/Announcements.tsx'
import AdminInstances from './pages/admin/Instances.tsx'
import AdminInstanceDetail from './pages/admin/InstanceDetail.tsx'
import AdminRelays from './pages/admin/Relays.tsx'
import {
  FaspDebugCallbacks as AdminFaspDebugCallbacks,
  FaspProviderEdit as AdminFaspProviderEdit,
  FaspProviders as AdminFaspProviders,
  FaspRegistration as AdminFaspRegistration,
} from './pages/admin/Fasp.tsx'
import AdminInvites from './pages/admin/AdminInvites.tsx'
import AdminWebhooks from './pages/admin/Webhooks.tsx'
import AdminFollowRecommendations from './pages/admin/FollowRecommendations.tsx'
import AdminSoftwareUpdates from './pages/admin/SoftwareUpdates.tsx'
import Strikes from './pages/Strikes.tsx'
import StrikeDetail from './pages/StrikeDetail.tsx'
import { ThemeProvider } from './components/theme-provider.tsx'
import { ComposeModalProvider } from './components/compose-modal.tsx'
import { Toaster } from './components/ui/sonner.tsx'
import { getToken } from './auth.ts'
import { loadMe } from './me.ts'
import './styles.css'

// Warm the current-user id cache for edit/delete controls.
const token = getToken()
if (token) void loadMe(token)

// Static routes outrank dynamic ones in React Router's ranking, so
// `/auth/callback`, `/local`, etc. are matched before `/:acct`. `:acct`
// captures the `@username` (or `@username@domain`) segment of Mastodon-style
// permalinks — bare words like `/local` never collide because profiles carry
// the `@` prefix.
const router = createBrowserRouter([
  { path: '/', element: <Home /> },
  { path: '/auth/callback', element: <Callback /> },
  { path: '/local', element: <PublicTimeline /> },
  { path: '/public', element: <PublicTimeline /> },
  { path: '/notifications', element: <Notifications /> },
  { path: '/follow-requests', element: <FollowRequests /> },
  // Mastodon's address for it, which its notification emails link.
  { path: '/follow_requests', element: <Navigate to="/follow-requests" replace /> },
  { path: '/search', element: <SearchPage /> },
  { path: '/about', element: <About /> },
  // Mastodon's policy pages, which `urls.privacy_policy` and
  // `urls.terms_of_service` point at; `/terms` is its old address.
  { path: '/privacy-policy', element: <PrivacyPolicy /> },
  { path: '/terms-of-service', element: <TermsOfService /> },
  { path: '/terms-of-service/:date', element: <TermsOfService /> },
  { path: '/terms', element: <Navigate to="/terms-of-service" replace /> },
  { path: '/invite-tree', element: <InviteTree /> },
  { path: '/invites', element: <Invites /> },
  { path: '/signup', element: <Signup /> },
  { path: '/settings', element: <Settings /> },
  // Mastodon's import and export pages, at its paths.
  { path: '/settings/export', element: <ImportExport /> },
  { path: '/settings/imports', element: <ImportExport /> },
  { path: '/bookmarks', element: <Bookmarks /> },
  { path: '/messages', element: <Messages /> },
  { path: '/explore', element: <Explore /> },
  { path: '/explore/tags', element: <Explore /> },
  { path: '/explore/links', element: <Explore /> },
  { path: '/blocked', element: <BlockedAccounts /> },
  { path: '/muted', element: <BlockedAccounts /> },
  { path: '/tags/:name', element: <TagTimeline /> },
  // Moderation, at Mastodon's own admin paths so the links its notifications
  // and emails carry — `/admin/reports/:id`, `/admin/accounts/:id` — land on
  // the matching page. Every segment is static or comes after one, so none of
  // these compete with the profile and thread routes below.
  { path: '/admin', element: <AdminIndex /> },
  { path: '/admin/dashboard', element: <AdminDashboard /> },
  { path: '/admin/reports', element: <AdminReports /> },
  { path: '/admin/reports/:id', element: <AdminReportDetail /> },
  { path: '/admin/accounts', element: <AdminAccounts /> },
  { path: '/admin/accounts/:id', element: <AdminAccountDetail /> },
  { path: '/admin/domain_blocks', element: <AdminDomainBlocks /> },
  { path: '/admin/domain_allows', element: <AdminDomainAllows /> },
  { path: '/admin/ip_blocks', element: <AdminIpBlocks /> },
  { path: '/admin/email_domain_blocks', element: <AdminEmailDomainBlocks /> },
  { path: '/admin/canonical_email_blocks', element: <AdminCanonicalEmailBlocks /> },
  { path: '/admin/trends/links', element: <AdminTrends /> },
  { path: '/admin/trends/links/preview_card_providers', element: <AdminTrends /> },
  { path: '/admin/trends/statuses', element: <AdminTrends /> },
  { path: '/admin/trends/tags', element: <AdminTrends /> },
  { path: '/admin/tags', element: <AdminTags /> },
  { path: '/admin/custom_emojis', element: <AdminCustomEmojis /> },
  { path: '/admin/email_subscriptions', element: <AdminEmailSubscriptions /> },
  {
    path: '/admin/email_subscriptions/accounts/:id',
    element: <AdminEmailSubscriptionAccount />,
  },
  { path: '/admin/terms_of_service', element: <AdminTermsOfServiceIndex /> },
  { path: '/admin/terms_of_service/draft', element: <AdminTermsOfServiceDraft /> },
  { path: '/admin/terms_of_service/history', element: <AdminTermsOfServiceHistory /> },
  { path: '/admin/terms_of_service/generate', element: <AdminTermsOfServiceGenerate /> },
  { path: '/admin/terms_of_service/:id/preview', element: <AdminTermsOfServicePreview /> },
  { path: '/admin/action_logs', element: <AdminActionLogs /> },
  { path: '/admin/disputes/appeals', element: <AdminAppeals /> },
  { path: '/admin/warning_presets', element: <AdminWarningPresets /> },
  { path: '/admin/username_blocks', element: <AdminUsernameBlocks /> },
  { path: '/admin/accounts/:id/statuses', element: <AdminAccountStatuses /> },
  { path: '/admin/accounts/:id/statuses/:statusId', element: <AdminAccountStatusDetail /> },
  { path: '/admin/accounts/:id/relationships', element: <AdminRelationships /> },
  // Server administration, at Mastodon's admin paths.
  { path: '/admin/settings', element: <AdminSettings /> },
  { path: '/admin/settings/:page', element: <AdminSettings /> },
  { path: '/admin/rules', element: <AdminRules /> },
  { path: '/admin/roles', element: <AdminRoles /> },
  { path: '/admin/announcements', element: <AdminAnnouncements /> },
  { path: '/admin/instances', element: <AdminInstances /> },
  { path: '/admin/instances/:domain', element: <AdminInstanceDetail /> },
  { path: '/admin/relays', element: <AdminRelays /> },
  { path: '/admin/fasp/providers', element: <AdminFaspProviders /> },
  { path: '/admin/fasp/providers/:id/edit', element: <AdminFaspProviderEdit /> },
  // Where a provider's registration answer sends the administrator.
  { path: '/admin/fasp/providers/:id/registration/new', element: <AdminFaspRegistration /> },
  { path: '/admin/fasp/debug/callbacks', element: <AdminFaspDebugCallbacks /> },
  { path: '/admin/invites', element: <AdminInvites /> },
  { path: '/admin/webhooks', element: <AdminWebhooks /> },
  { path: '/admin/follow_recommendations', element: <AdminFollowRecommendations /> },
  { path: '/admin/software_updates', element: <AdminSoftwareUpdates /> },
  // Strikes and appeals, at Mastodon's paths, which its moderation warning
  // notifications and emails link to.
  { path: '/disputes/strikes', element: <Strikes /> },
  { path: '/disputes/strikes/:id', element: <StrikeDetail /> },
  { path: '/:acct', element: <Profile /> },
  // Static second segments outrank the dynamic `:id` thread route, so these
  // win over `/:acct/:id` (status ids are numeric and never collide).
  { path: '/:acct/followers', element: <AccountList /> },
  { path: '/:acct/following', element: <AccountList /> },
  { path: '/:acct/:id', element: <StatusThread /> },
  // Three segments, so these never compete with `/:acct/:id` above.
  { path: '/:acct/:id/favourites', element: <StatusReactions /> },
  { path: '/:acct/:id/reblogs', element: <StatusReactions /> },
  { path: '/:acct/:id/history', element: <StatusHistory /> },
  { path: '*', element: <Home /> },
])

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <ThemeProvider defaultTheme="system" storageKey="eunha-theme">
      <ComposeModalProvider>
        <RouterProvider router={router} />
        <TermsOfServiceInterstitial />
        <Toaster />
      </ComposeModalProvider>
    </ThemeProvider>
  </StrictMode>,
)
