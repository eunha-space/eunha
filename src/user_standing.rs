//! What Mastodon's `User` says about whether its owner may use the account:
//! `functional?`, `functional_or_moved?` and `missing_2fa?`, which
//! `require_functional!`, `require_user!`, invites, notification emails and
//! the feed settings all ask.
//!
//! Mastodon: app/models/user.rb

use sqlx::PgPool;

/// The facts `User#functional?` is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserStanding {
    pub confirmed: bool,
    pub approved: bool,
    pub disabled: bool,
    /// `Account#unavailable?`: suspended, or deletion requested.
    pub unavailable: bool,
    /// `Account#deleted?`: deletion requested.
    pub deleted: bool,
    pub memorial: bool,
    pub moved: bool,
    /// `User#missing_2fa?`: the role (the everyone role when it has none)
    /// requires two-factor authentication, and the user has neither an
    /// authenticator app nor a security key.
    pub missing_2fa: bool,
}

impl UserStanding {
    /// `User#nonfunctional_reason`: the first thing that keeps the user from
    /// the account, as `Account#state` names it.
    pub fn nonfunctional_reason(&self) -> Option<&'static str> {
        if !self.confirmed {
            Some("pending_confirmation")
        } else if !self.approved {
            Some("pending_approval")
        } else if self.deleted {
            Some("account_deleted")
        } else if self.unavailable {
            Some("account_suspended")
        } else if self.memorial {
            Some("memorial")
        } else if self.disabled {
            Some("login_disabled")
        } else if self.missing_2fa {
            Some("2fa_required")
        } else {
            None
        }
    }

    /// `User#functional_or_moved?`.
    pub fn functional_or_moved(&self) -> bool {
        self.nonfunctional_reason().is_none()
    }

    /// `User#functional?`.
    pub fn functional(&self) -> bool {
        self.functional_or_moved() && !self.moved
    }

    /// The standing of the local user behind `account_id`, if there is one.
    pub async fn of_account(db: &PgPool, account_id: i64) -> sqlx::Result<Option<Self>> {
        Self::load(db, None, Some(account_id)).await
    }

    /// The standing of the user `user_id`, if there is one.
    pub async fn of_user(db: &PgPool, user_id: i64) -> sqlx::Result<Option<Self>> {
        Self::load(db, Some(user_id), None).await
    }

    async fn load(
        db: &PgPool,
        user_id: Option<i64>,
        account_id: Option<i64>,
    ) -> sqlx::Result<Option<Self>> {
        let row = sqlx::query!(
            r#"SELECT u.confirmed_at IS NOT NULL AS "confirmed!", u.approved, u.disabled,
                      (a.suspended_at IS NOT NULL OR a.requested_deletion_at IS NOT NULL)
                        AS "unavailable!",
                      a.requested_deletion_at IS NOT NULL AS "deleted!",
                      a.memorial, a.moved_to_account_id IS NOT NULL AS "moved!",
                      (COALESCE(r.require_2fa, false) AND NOT u.otp_required_for_login
                       AND NOT EXISTS (SELECT 1 FROM webauthn_credentials w
                                       WHERE w.user_id = u.id)) AS "missing_2fa!"
               FROM users u
               JOIN accounts a ON a.id = u.account_id
               LEFT JOIN user_roles r ON r.id = COALESCE(u.role_id, -99)
               WHERE ($1::bigint IS NULL OR u.id = $1) AND ($2::bigint IS NULL OR u.account_id = $2)
               LIMIT 1"#,
            user_id,
            account_id,
        )
        .fetch_optional(db)
        .await?;
        Ok(row.map(|row| Self {
            confirmed: row.confirmed,
            approved: row.approved,
            disabled: row.disabled,
            unavailable: row.unavailable,
            deleted: row.deleted,
            memorial: row.memorial,
            moved: row.moved,
            missing_2fa: row.missing_2fa,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::UserStanding;

    #[test]
    fn functional_is_mastodons() {
        let ok = UserStanding {
            confirmed: true,
            approved: true,
            disabled: false,
            unavailable: false,
            deleted: false,
            memorial: false,
            moved: false,
            missing_2fa: false,
        };
        assert!(ok.functional());
        let moved = UserStanding { moved: true, ..ok };
        assert!(!moved.functional() && moved.functional_or_moved());
        for not in [
            UserStanding {
                confirmed: false,
                ..ok
            },
            UserStanding {
                approved: false,
                ..ok
            },
            UserStanding {
                disabled: true,
                ..ok
            },
            UserStanding {
                unavailable: true,
                deleted: false,
                ..ok
            },
            UserStanding {
                memorial: true,
                ..ok
            },
            UserStanding {
                missing_2fa: true,
                ..ok
            },
        ] {
            assert!(!not.functional_or_moved(), "{not:?}");
        }
    }
}
