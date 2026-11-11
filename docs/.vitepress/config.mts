import { defineConfig } from "vitepress";

// Served from eunha.social/docs/, beside the hand-written landing page in
// site/, which the Pages workflow deploys with this build copied under it.
export default defineConfig({
  base: "/docs/",
  title: "eunha",
  description: "Mastodon, reimplemented in Rust",
  lang: "en",
  cleanUrls: true,
  lastUpdated: true,
  head: [["link", { rel: "icon", href: "/docs/favicon.svg", type: "image/svg+xml" }]],
  themeConfig: {
    logo: "/favicon.svg",
    nav: [
      { text: "eunha.social", link: "https://eunha.social/" },
    ],
    sidebar: [
      {
        text: "Getting started",
        items: [
          { text: "Download eunha", link: "/operating/downloads" },
          { text: "Try eunha", link: "/operating/trying" },
          { text: "Container images", link: "/operating/containers" },
          { text: "The first account", link: "/operating/first-account" },
        ],
      },
      {
        text: "Running eunha",
        items: [
          { text: "Migrations", link: "/operating/migrations" },
          { text: "Importing a Mastodon instance", link: "/operating/importing" },
          { text: "Several instances in one process", link: "/operating/instances" },
          { text: "Shared Redis", link: "/operating/redis" },
          { text: "The job queue", link: "/operating/jobs" },
          { text: "Update notices", link: "/operating/update-notices" },
          { text: "Self-destruct", link: "/operating/self-destruct" },
        ],
      },
      {
        text: "Instance administration",
        items: [
          { text: "Administration", link: "/operating/administration" },
          { text: "Invites", link: "/operating/invites" },
          { text: "Moderation", link: "/operating/moderation" },
          { text: "Rate limits", link: "/operating/rate-limits" },
          { text: "Terms of service and privacy policy", link: "/operating/terms-of-service" },
          { text: "Maintenance commands", link: "/operating/maintenance" },
          { text: "Accounts, domains and emoji", link: "/operating/maintenance-commands" },
        ],
      },
      {
        text: "Member accounts and data",
        items: [
          { text: "Account security", link: "/operating/accounts" },
          { text: "Account moves", link: "/operating/account-moves" },
          { text: "Data export and import", link: "/operating/import-export" },
          { text: "Email subscriptions", link: "/operating/email-subscriptions" },
        ],
      },
      {
        text: "Content and integrations",
        items: [
          { text: "Media", link: "/operating/media" },
          { text: "Image viewer", link: "/operating/media-viewer" },
          { text: "Preview cards", link: "/operating/preview-cards" },
          { text: "Remote replies", link: "/operating/remote-replies" },
          { text: "Search", link: "/operating/search" },
          { text: "Translation", link: "/operating/translation" },
          { text: "Auxiliary service providers", link: "/operating/fasp" },
        ],
      },
      {
        text: "Mastodon compatibility",
        items: [
          { text: "Tracking Mastodon", link: "/mastodon/tracking" },
          { text: "Deliberate divergences", link: "/mastodon/divergences" },
          { text: "Outstanding from 5.0.0-beta.1", link: "/mastodon/5.0.0-beta.1" },
          { text: "API entity parity", link: "/mastodon/entity-parity" },
          { text: "Differential testing", link: "/mastodon/differential-testing" },
          { text: "Federating with Mastodon", link: "/mastodon/federation-testing" },
        ],
      },
      {
        text: "ActivityPub federation",
        items: [
          { text: "Signing keys", link: "/mastodon/signing-keys" },
          { text: "HTTP signatures", link: "/mastodon/http-signatures" },
          { text: "Remote actors", link: "/mastodon/remote-actors" },
          { text: "What other servers fetch", link: "/mastodon/serving" },
          { text: "Inbound statuses", link: "/mastodon/inbound-statuses" },
          { text: "Quotes", link: "/mastodon/quotes" },
        ],
      },
      {
        text: "Posts and client APIs",
        items: [
          { text: "Posting and editing", link: "/mastodon/statuses" },
          { text: "Content formatting", link: "/mastodon/formatting" },
          { text: "Streaming", link: "/mastodon/streaming" },
        ],
      },
      {
        text: "Design",
        items: [
          { text: "Protocol extension", link: "/design/protocol" },
          { text: "Benchmarking", link: "/design/benchmarking" },
        ],
      },
      {
        text: "Contributing",
        items: [
          { text: "Contributing", link: "/contributing/" },
          { text: "Where to pick this up", link: "/contributing/next" },
        ],
      },
    ],
    outline: "deep",
    search: { provider: "local" },
    editLink: {
      pattern: "https://github.com/limeburst/eunha/edit/main/docs/:path",
    },
    socialLinks: [
      { icon: "github", link: "https://github.com/limeburst/eunha" },
      { icon: "discord", link: "https://discord.gg/kvEKHhgANs" },
    ],
    footer: {
      message:
        'Licensed under the <a href="https://github.com/limeburst/eunha/blob/main/LICENSE">GNU AGPL-3.0</a>. Not affiliated with Mastodon gGmbH.',
    },
  },
});
