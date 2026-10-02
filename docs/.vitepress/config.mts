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
        text: "Running eunha",
        items: [
          { text: "The first account", link: "/operating/first-account" },
          { text: "Migrations", link: "/operating/migrations" },
          { text: "Importing a Mastodon instance", link: "/operating/importing" },
          { text: "Shared Redis", link: "/operating/redis" },
          { text: "Several instances in one process", link: "/operating/instances" },
          { text: "Invites", link: "/operating/invites" },
          { text: "Moderation", link: "/operating/moderation" },
          { text: "Email subscriptions", link: "/operating/email-subscriptions" },
          { text: "Terms of service and privacy policy", link: "/operating/terms-of-service" },
          { text: "Account moves", link: "/operating/account-moves" },
          { text: "Data export and import", link: "/operating/import-export" },
          { text: "Preview cards", link: "/operating/preview-cards" },
          { text: "Remote replies", link: "/operating/remote-replies" },
          { text: "Update notices", link: "/operating/update-notices" },
        ],
      },
      {
        text: "Mastodon compatibility",
        items: [
          { text: "Tracking Mastodon", link: "/mastodon/tracking" },
          { text: "Signing keys", link: "/mastodon/signing-keys" },
          { text: "HTTP signatures", link: "/mastodon/http-signatures" },
          { text: "API entity parity", link: "/mastodon/entity-parity" },
          { text: "Differential testing", link: "/mastodon/differential-testing" },
          { text: "Federating with Mastodon", link: "/mastodon/federation-testing" },
          { text: "Deliberate divergences", link: "/mastodon/divergences" },
          { text: "Outstanding from 4.7.1", link: "/mastodon/4.7.1" },
        ],
      },
      {
        text: "Design",
        items: [
          { text: "Protocol extension", link: "/design/protocol" },
          { text: "Hosted tenancy plan", link: "/design/multitenancy" },
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
