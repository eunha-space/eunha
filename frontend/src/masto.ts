// masto.js is our Mastodon C2S API SDK. The SPA is served same-origin by the
// eunha server, so the API base URL is just the current origin.
import {
  createOAuthAPIClient,
  createRestAPIClient,
  createStreamingAPIClient,
  type mastodon,
} from 'masto'

const url = window.location.origin
const streamingApiUrl = `${window.location.protocol === 'https:' ? 'wss:' : 'ws:'}//${window.location.host}`

export const restClient = (accessToken?: string) =>
  createRestAPIClient({ url, accessToken })

export const oauthClient = () => createOAuthAPIClient({ url })

export const streamingClient = (accessToken?: string) =>
  createStreamingAPIClient({ streamingApiUrl, accessToken })

export type { mastodon }
