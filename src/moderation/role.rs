//! Mastodon's `UserRole`, and the role a policy check judges by.

use sqlx::PgPool;

use crate::error::{AppError, AppResult};

/// `UserRole::FLAGS`.
pub mod flag {
    pub const ADMINISTRATOR: i64 = 1 << 0;
    pub const VIEW_DEVOPS: i64 = 1 << 1;
    pub const VIEW_AUDIT_LOG: i64 = 1 << 2;
    pub const VIEW_DASHBOARD: i64 = 1 << 3;
    pub const MANAGE_REPORTS: i64 = 1 << 4;
    pub const MANAGE_FEDERATION: i64 = 1 << 5;
    pub const MANAGE_SETTINGS: i64 = 1 << 6;
    pub const MANAGE_BLOCKS: i64 = 1 << 7;
    pub const MANAGE_TAXONOMIES: i64 = 1 << 8;
    pub const MANAGE_APPEALS: i64 = 1 << 9;
    pub const MANAGE_USERS: i64 = 1 << 10;
    pub const MANAGE_INVITES: i64 = 1 << 11;
    pub const MANAGE_RULES: i64 = 1 << 12;
    pub const MANAGE_ANNOUNCEMENTS: i64 = 1 << 13;
    pub const MANAGE_CUSTOM_EMOJIS: i64 = 1 << 14;
    pub const MANAGE_WEBHOOKS: i64 = 1 << 15;
    pub const INVITE_USERS: i64 = 1 << 16;
    pub const MANAGE_ROLES: i64 = 1 << 17;
    pub const MANAGE_USER_ACCESS: i64 = 1 << 18;
    pub const DELETE_USER_DATA: i64 = 1 << 19;
    pub const VIEW_FEEDS: i64 = 1 << 20;
    pub const INVITE_BYPASS_APPROVAL: i64 = 1 << 21;
    pub const MANAGE_EMAIL_SUBSCRIPTIONS: i64 = 1 << 22;

    /// `Flags::ALL`.
    pub const ALL: i64 = (1 << 23) - 1;
    /// `Flags::DEFAULT`, what `UserRole.everyone` is created with.
    pub const DEFAULT: i64 = INVITE_USERS;

    /// `Flags::CATEGORIES[:moderation]`.
    pub const MODERATION: &[i64] = &[
        VIEW_DASHBOARD,
        VIEW_AUDIT_LOG,
        MANAGE_USERS,
        MANAGE_USER_ACCESS,
        DELETE_USER_DATA,
        MANAGE_REPORTS,
        MANAGE_APPEALS,
        MANAGE_FEDERATION,
        MANAGE_BLOCKS,
        MANAGE_TAXONOMIES,
        MANAGE_INVITES,
        VIEW_FEEDS,
    ];
}

/// `UserRole::EVERYONE_ROLE_ID`.
pub const EVERYONE_ROLE_ID: i64 = -99;
/// `UserRole::NOBODY_POSITION`, which the everyone role is also given.
pub const NOBODY_POSITION: i32 = -1;

/// A `user_roles` row, or the unsaved `UserRole.nobody`.
#[derive(Debug, Clone)]
pub struct Role {
    /// `None` for `UserRole.nobody`.
    pub id: Option<i64>,
    pub name: String,
    pub color: String,
    pub position: i32,
    pub permissions: i64,
    pub highlighted: bool,
    pub collection_limit: i32,
    /// `computed_permissions`.
    pub computed: i64,
}

impl Role {
    /// `UserRole.nobody`.
    pub fn nobody() -> Self {
        Self {
            id: None,
            name: String::new(),
            color: String::new(),
            position: NOBODY_POSITION,
            permissions: 0,
            highlighted: false,
            collection_limit: 10,
            computed: 0,
        }
    }

    pub fn is_everyone(&self) -> bool {
        self.id == Some(EVERYONE_ROLE_ID)
    }

    /// `can?(*any_of_privileges)`.
    pub fn can(&self, any_of: &[i64]) -> bool {
        any_of.iter().any(|flag| self.computed & flag == *flag)
    }

    /// `overrides?(other_role)`: a role acts on a lower one only, and on an
    /// account with no role at all (a remote one) always.
    pub fn overrides(&self, other: Option<&Role>) -> bool {
        other.is_none_or(|other| self.position > other.position)
    }

    /// `bypass_block?(role)`.
    pub fn bypass_block(&self, other: Option<&Role>) -> bool {
        self.overrides(other) && self.highlighted && self.can(flag::MODERATION)
    }
}

struct RoleRow {
    id: i64,
    name: String,
    color: String,
    position: i32,
    permissions: i64,
    highlighted: bool,
    collection_limit: i32,
}

async fn everyone_permissions(db: &PgPool) -> AppResult<i64> {
    // `UserRole.everyone` creates the row when it is missing, with
    // `Flags::DEFAULT`; reading it as that is the same answer.
    Ok(sqlx::query_scalar!(
        "SELECT permissions FROM user_roles WHERE id = $1",
        EVERYONE_ROLE_ID,
    )
    .fetch_optional(db)
    .await?
    .unwrap_or(flag::DEFAULT))
}

async fn from_row(db: &PgPool, row: RoleRow) -> AppResult<Role> {
    let everyone = row.id == EVERYONE_ROLE_ID;
    // `computed_permissions`: the everyone role is just its own, any other
    // unions the everyone role's in, and `administrator` is everything.
    let computed = if everyone {
        row.permissions
    } else if row.permissions & flag::ADMINISTRATOR != 0 {
        flag::ALL
    } else {
        row.permissions | everyone_permissions(db).await?
    };
    Ok(Role {
        id: Some(row.id),
        name: row.name,
        color: row.color,
        // `set_position` keeps the everyone role at the nobody position.
        position: if everyone {
            NOBODY_POSITION
        } else {
            row.position
        },
        permissions: row.permissions,
        highlighted: row.highlighted,
        collection_limit: row.collection_limit,
        computed,
    })
}

/// `Account#user_role`: the role of the account's user, the everyone role for a
/// user without one, and `None` for an account without a user (a remote one).
pub async fn of_account(db: &PgPool, account_id: i64) -> AppResult<Option<Role>> {
    let row = sqlx::query_as!(
        RoleRow,
        r#"SELECT ur.id, ur.name, ur.color, ur.position, ur.permissions,
                  ur.highlighted, ur.collection_limit
           FROM users u
           JOIN user_roles ur ON ur.id = COALESCE(u.role_id, $2)
           WHERE u.account_id = $1"#,
        account_id,
        EVERYONE_ROLE_ID,
    )
    .fetch_optional(db)
    .await?;
    match row {
        Some(row) => Ok(Some(from_row(db, row).await?)),
        // No user, or a user whose everyone role row is missing.
        None => {
            let has_user = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM users WHERE account_id = $1) AS "e!""#,
                account_id,
            )
            .fetch_one(db)
            .await?;
            Ok(has_user.then(|| Role {
                id: Some(EVERYONE_ROLE_ID),
                computed: flag::DEFAULT,
                permissions: flag::DEFAULT,
                ..Role::nobody()
            }))
        }
    }
}

/// `ApplicationPolicy#role`: the role a policy judges the acting account by,
/// which is nobody's when its user is disabled.
pub async fn acting(db: &PgPool, account_id: i64) -> AppResult<Role> {
    let disabled = sqlx::query_scalar!(
        "SELECT disabled FROM users WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(db)
    .await?;
    match disabled {
        None | Some(true) => Ok(Role::nobody()),
        Some(false) => Ok(of_account(db, account_id)
            .await?
            .unwrap_or_else(Role::nobody)),
    }
}

/// `authorize` with a role check: 403 unless `allowed`.
pub fn authorize(allowed: bool) -> AppResult<()> {
    if allowed {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(position: i32, computed: i64) -> Role {
        Role {
            id: Some(1),
            position,
            computed,
            ..Role::nobody()
        }
    }

    #[test]
    fn overrides_only_lower_positions_and_roleless_accounts() {
        let moderator = role(10, flag::MANAGE_REPORTS);
        assert!(moderator.overrides(None));
        assert!(moderator.overrides(Some(&role(-1, 0))));
        assert!(!moderator.overrides(Some(&role(10, 0))));
        assert!(!moderator.overrides(Some(&role(100, 0))));
    }

    #[test]
    fn can_any_of() {
        let moderator = role(10, flag::MANAGE_REPORTS);
        assert!(moderator.can(&[flag::MANAGE_USERS, flag::MANAGE_REPORTS]));
        assert!(!moderator.can(&[flag::MANAGE_USERS]));
        assert!(!Role::nobody().can(&[flag::MANAGE_USERS]));
    }
}
