//! Moderation: Mastodon's roles and policies, the admin action log, account
//! actions and strikes, and what suspending an account sets in motion.

pub mod account_action;
pub mod action_log;
pub mod domain_block;
pub mod history;
pub mod notification_policy;
pub mod remote;
pub mod report_service;
pub mod role;
pub mod rules;
pub mod severance;
pub mod signup;
pub mod suspension;
pub mod warning;
pub mod webhooks;

/// `Report::COMMENT_SIZE_LIMIT`, for local reports.
pub const COMMENT_SIZE_LIMIT: usize = 1_000;
