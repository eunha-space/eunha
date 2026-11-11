//! Generates the secrets Mastodon calls `ACTIVE_RECORD_ENCRYPTION_*`.
//!
//! Mastodon generates these with `bin/rails db:encryption:init`; eunha needs
//! the same values to read and write `keypairs.private_key`, and an instance
//! that was never a Mastodon has no Rails to run that task in.
//!
//! An instance migrating *from* Mastodon must not use these: copy that
//! installation's existing values instead, or its stored keys become
//! unreadable. Once an instance has encrypted anything, changing them is data
//! loss.
//!
//!   eunha-generate-secrets            # .env lines, ready to paste
//!   eunha-generate-secrets --toml     # a config.toml section
//!
//! Mastodon: config/initializers/active_record_encryption.rb

use clap::Parser;
use eunha::rails_encryption::generate_secret;

#[derive(Parser, Debug)]
#[command(
    name = "eunha-generate-secrets",
    about = "Generate ActiveRecord encryption secrets for a new instance"
)]
struct Args {
    /// Print a `config.toml` section instead of environment variables.
    #[arg(long)]
    toml: bool,
}

fn main() {
    let args = Args::parse();

    let primary_key = generate_secret();
    let deterministic_key = generate_secret();
    let key_derivation_salt = generate_secret();

    if args.toml {
        println!("[active_record_encryption]");
        println!("primary_key = \"{primary_key}\"");
        println!("key_derivation_salt = \"{key_derivation_salt}\"");
        println!();
        println!("# Mastodon refuses to boot without a deterministic key, though");
        println!("# nothing in its schema is encrypted deterministically and eunha");
        println!("# never reads it. Keep it with the others.");
        println!("# deterministic_key = \"{deterministic_key}\"");
    } else {
        println!("ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY={primary_key}");
        println!("ACTIVE_RECORD_ENCRYPTION_DETERMINISTIC_KEY={deterministic_key}");
        println!("ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT={key_derivation_salt}");
    }

    eprintln!();
    eprintln!("Store these somewhere they cannot be lost: the signing keys they");
    eprintln!("encrypt cannot be recovered without them. If this database came from");
    eprintln!("a Mastodon installation, use that installation's values instead.");
}
