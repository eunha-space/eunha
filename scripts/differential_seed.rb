# Seeds and resets the Mastodon side of the differential harness, and prints
# what the comparison needs to know about it as one JSON line.
#
# Run by `differential_test.sh` through `bin/rails runner`. The eunha side gets
# a scratch database every run; this container is left up between runs, so
# everything a flow changes has to be put back here or the second run compares
# against what the first one left — which reads exactly like eunha differing.

def token_for(user, app_name, scopes)
  app = Doorkeeper::Application.find_or_create_by!(name: app_name) do |a|
    a.redirect_uri = 'urn:ietf:wg:oauth:2.0:oob'
    a.scopes = scopes
  end
  app.update!(scopes: scopes) if app.scopes.to_s != scopes
  token = Doorkeeper::AccessToken.where(application: app, resource_owner_id: user.id, revoked_at: nil)
                                 .find { |t| t.scopes.to_s == scopes } ||
          Doorkeeper::AccessToken.create!(application: app, resource_owner_id: user.id,
                                          scopes: scopes, expires_in: nil)
  token.token
end

def local_account(name, role: nil, scopes: 'read write follow push')
  account = Account.find_or_create_by!(username: name, domain: nil)
  user = User.find_by(email: "#{name}@localhost") || User.create!(
    email: "#{name}@localhost", password: SecureRandom.hex(16), account: account,
    agreement: true, approved: true, confirmed_at: Time.now.utc
  )
  # A seeded user is unapproved, and an unapproved user answers 403 to every
  # authenticated endpoint — which looks like agreement if only status codes
  # are compared.
  user.update!(approved: true, confirmed_at: Time.now.utc, disabled: false,
               role: role ? UserRole.find_by!(name: role) : UserRole.everyone)
  [account, token_for(user, "app-#{name}", scopes)]
end

def remote_account(name, domain)
  Account.find_or_create_by!(username: name, domain: domain) do |a|
    a.uri = "https://#{domain}/users/#{name}"
    a.url = "https://#{domain}/@#{name}"
    a.inbox_url = "https://#{domain}/users/#{name}/inbox"
    a.shared_inbox_url = "https://#{domain}/inbox"
    a.followers_url = "https://#{domain}/users/#{name}/followers"
    a.protocol = :activitypub
    a.actor_type = 'Person'
  end
end

ids = {}
tokens = {}

%w(differ other fan1 fan2 fan3 troll mover moved_to).each do |name|
  account, token = local_account(name)
  ids[name] = account.id.to_s
  tokens[name] = token
end
# The moderator: an Owner, with the admin scopes. Separate from `differ` so the
# role does not show up in the entities every other comparison reads.
mod, mod_token = local_account('warden', role: 'Owner',
                                      scopes: 'read write follow push admin:read admin:write')
ids['mod'] = mod.id.to_s
tokens['mod'] = mod_token

differ = Account.find(ids['differ'])
# More tokens for `differ`, one taken per flow: `throttle_per_token_api`
# allows a token 300 requests in five minutes, and the flows between them
# make more than that — at which point both servers answer 429 to
# everything, agree perfectly, and compare nothing.
flow_tokens = (1..6).map do |i|
  token_for(differ.user, "app-differ-flow-#{i}", 'read write follow push')
end

# And the counts from earlier runs, which this container keeps: the
# `Rack::Attack` throttles in the cache and the `RateLimiter` families —
# posting, following, reporting — each counted over hours.
RedisConnection.with do |redis|
  %w(rate_limit:* cache:rack::attack:*).each do |pattern|
    redis.scan_each(match: pattern).each_slice(100) { |keys| redis.del(*keys) }
  end
end
troll = Account.find(ids['troll'])
mover = Account.find(ids['mover'])
moved_to = Account.find(ids['moved_to'])

faraway = remote_account('faraway', 'blocked.example')
distant = remote_account('distant', 'silenced.example')
ids['faraway'] = faraway.id.to_s
ids['distant'] = distant.id.to_s

# ── Domain blocks: back to none ─────────────────────────────────────────────
AccountDomainBlock.where(account: differ).destroy_all
DomainBlock.where(domain: %w(silenced.example blocked.example)).find_each do |block|
  UnblockDomainService.new.call(block)
end
# A suspension by domain block runs `DeleteAccountService`, which the unblock
# above does not undo; and an updated block's retroactive pass only clears what
# it stamped. Whatever was left, clear it.
[faraway, distant].each do |a|
  a.update_columns(suspended_at: nil, suspension_origin: nil, silenced_at: nil)
end
RelationshipSeveranceEvent.where(target_name: %w(silenced.example blocked.example)).destroy_all

# A remote follower on the domain `differ` is about to block, with the follow
# notification it produced, and a remote account `differ` follows on the domain
# the moderator is about to suspend: what each block removes.
follow = Follow.find_or_create_by!(account: faraway, target_account: differ)
Follow.find_or_create_by!(account: differ, target_account: distant)
Notification.where(account: differ).delete_all
Notification.create!(account: differ, activity: follow, type: :follow)

# ── Moderation: an untouched troll ──────────────────────────────────────────
troll.unsuspend! if troll.suspended?
troll.update_columns(silenced_at: nil, sensitized_at: nil)
troll.user.update_columns(disabled: false)
Report.where(target_account: troll).destroy_all
AccountWarning.where(target_account: troll).destroy_all
Notification.where(account: troll).delete_all

rules = ['No spam', 'Be kind'].map { |text| Rule.find_or_create_by!(text: text) }

# ── Moves: an account that has moved ────────────────────────────────────────
mover.update!(moved_to_account: moved_to)

# ── Per-account state the broader flows create ──────────────────────────────
# `update_credentials` changes the profile every status embeds, so a second
# run would compare `account.note` on every write against the first's.
differ.update!(note: '', locked: false, bot: false, fields: [])
CustomFilter.where(account: differ).destroy_all
List.where(account: differ).destroy_all
ScheduledStatus.where(account: differ).destroy_all
Marker.where(user: differ.user).delete_all
TagFollow.where(account: differ).destroy_all
FeaturedTag.where(account: differ).destroy_all
AccountNote.where(account: differ).delete_all
AccountPin.where(account: differ).destroy_all
NotificationPolicy.where(account: differ).delete_all
Web::PushSubscription.where(user: differ.user).delete_all
Follow.where(account: differ, target_account_id: ids['other']).destroy_all
Follow.where(account_id: ids['other'], target_account: differ).destroy_all
AccountConversation.where(account_id: [differ.id, ids['other']]).delete_all
Notification.where(account_id: ids['other']).delete_all
NotificationPolicy.where(account_id: ids['other']).delete_all

puts "FIXTURE #{JSON.generate(ids: ids, tokens: tokens, flow_tokens: flow_tokens,
                              rules: rules.map { |r| r.id.to_s })}"
