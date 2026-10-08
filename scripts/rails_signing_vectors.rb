# frozen_string_literal: true

# Prints the test vectors in src/secret_key_base.rs: what a Mastodon 4.7.1
# (Rails 8.1, globalid 1.4, Devise 5.0) configured with a given
# SECRET_KEY_BASE signs and stores.
#
#   ruby scripts/rails_signing_vectors.rb [secret_key_base]
#
# Only Ruby's standard library is needed, so it runs where the gems are not
# installed. It restates, step by step, what these do under
# `config.load_defaults 8.1`:
#
#  -  `Rails.application.key_generator`: PBKDF2-HMAC-SHA256 over
#     secret_key_base, 1000 iterations, 64 bytes, salted with the verifier's
#     name (`key_generator_hash_digest_class` is SHA256 since 7.0).
#  -  `Rails.application.message_verifier(name)`: `ActiveSupport::
#     MessageVerifier` with that key, HMAC-SHA1, the `:json_allow_marshal`
#     serializer, metadata inside the serialized message
#     (`use_message_serializer_for_metadata`), strict base64.
#  -  `SignedGlobalID`: `GlobalID::Verifier` keyed for `signed_global_ids`,
#     the same as above but in URL-safe base64 with padding, the URI
#     `gid://mastodon/<Model>/<id>` with `pur` and an `exp` a month out.
#  -  `Devise.token_generator`: HMAC-SHA256 under a key that
#     `ActiveSupport::KeyGenerator.new(secret_key_base)` derives for
#     "Devise <column>". Devise builds that generator in an initializer,
#     before the `after_initialize` hook that switches KeyGenerator's class
#     default to SHA256, so it is PBKDF2-HMAC-SHA1 at the 2**16 iterations
#     that are KeyGenerator's own default.
#
# The same steps, run through the unmodified upstream files of those
# releases, gave identical output when the vectors were made.

require 'openssl'
require 'base64'
require 'json'
require 'time'

SECRET = ARGV[0] || ('0123456789abcdef' * 8)

def app_key(salt)
  OpenSSL::PKCS5.pbkdf2_hmac(SECRET, salt, 1000, 64, OpenSSL::Digest.new('SHA256'))
end

def devise_key(column)
  OpenSSL::PKCS5.pbkdf2_hmac(SECRET, "Devise #{column}", 2**16, 64, OpenSSL::Digest.new('SHA1'))
end

# `ActiveSupport::JSON.encode`: JSON with <, >, &, U+2028 and U+2029 escaped.
def as_json(value)
  escapes = {
    '<' => '\\u003c', '>' => '\\u003e', '&' => '\\u0026',
    [0x2028].pack('U') => '\\u2028', [0x2029].pack('U') => '\\u2029',
  }
  JSON.generate(value).gsub(Regexp.union(escapes.keys), escapes)
end

def sign(key, encoded)
  "#{encoded}--#{OpenSSL::HMAC.hexdigest('SHA1', key, encoded)}"
end

def message_verifier(name, value)
  sign(app_key(name), Base64.strict_encode64(as_json(value)))
end

def signed_global_id(gid, purpose, expires_at)
  envelope = { '_rails' => { 'data' => gid, 'exp' => expires_at.utc.iso8601(3), 'pur' => purpose } }
  sign(app_key('signed_global_ids'), Base64.urlsafe_encode64(as_json(envelope)))
end

def devise_digest(column, value)
  OpenSSL::HMAC.hexdigest('SHA256', devise_key(column), value)
end

expires_at = Time.utc(2026, 11, 2, 7, 25, 1, 123_456)
puts "secret_key_base: #{SECRET}"
puts "async_refreshes: #{message_verifier('async_refreshes', 'async_refreshes:v1:accounts:123:refresh_followers')}"
puts "self-destruct: #{message_verifier('self-destruct', 'example.com')}"
puts "sgid User/1: #{signed_global_id('gid://mastodon/User/1', 'unsubscribe', expires_at)}"
puts "sgid EmailSubscription/42: #{signed_global_id('gid://mastodon/EmailSubscription/42', 'unsubscribe', expires_at)}"
puts "reset_password_token: #{devise_digest('reset_password_token', 'sxyzAbCdEfGhIjKlMnOp')}"
