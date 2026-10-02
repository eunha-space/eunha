//! `Admin::RolesController`: creating, editing and deleting `user_roles`, as
//! `UserRolePolicy` allows and `UserRole`'s validations accept.

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, Params};
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        action_log::{self, Target},
        role::{self, flag, Role, EVERYONE_ROLE_ID, NOBODY_POSITION},
    },
    state::AppState,
};

/// `UserRole::POSITION_LIMIT`.
const POSITION_LIMIT: i64 = (1 << 31) - 1;

/// A role as the roles pages show it: `REST::RoleSerializer`'s fields, then
/// what the form edits and what the acting role may do with it.
#[derive(Debug, Serialize)]
pub struct AdminRole {
    #[serde(flatten)]
    pub role: super::accounts::RoleEntity,
    pub position: i32,
    pub require_2fa: bool,
    /// `permissions_as_keys`: the role's own flags, not the computed ones.
    pub permissions_as_keys: Vec<&'static str>,
    pub everyone: bool,
    /// How many users have the role; for the everyone role, those with none.
    pub users_count: i64,
    /// `UserRolePolicy#update?` for the acting role.
    pub can_update: bool,
    /// `UserRolePolicy#destroy?` for the acting role.
    pub can_destroy: bool,
    pub created_at: String,
    pub updated_at: String,
}

struct Row {
    id: i64,
    name: String,
    color: String,
    position: i32,
    permissions: i64,
    highlighted: bool,
    collection_limit: i32,
    require_2fa: bool,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

impl Row {
    fn role(&self, everyone_permissions: i64) -> Role {
        let everyone = self.id == EVERYONE_ROLE_ID;
        Role {
            id: Some(self.id),
            name: self.name.clone(),
            color: self.color.clone(),
            position: if everyone {
                NOBODY_POSITION
            } else {
                self.position
            },
            permissions: self.permissions,
            highlighted: self.highlighted,
            collection_limit: self.collection_limit,
            computed: if everyone {
                self.permissions
            } else if self.permissions & flag::ADMINISTRATOR != 0 {
                flag::ALL
            } else {
                self.permissions | everyone_permissions
            },
        }
    }
}

async fn everyone_permissions(state: &AppState) -> AppResult<i64> {
    Ok(sqlx::query_scalar!(
        "SELECT permissions FROM user_roles WHERE id = $1",
        EVERYONE_ROLE_ID
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(flag::DEFAULT))
}

/// `UserRolePolicy#update?`.
fn can_update(acting: &Role, record: &Role) -> bool {
    acting.can(&[flag::MANAGE_ROLES]) && (acting.overrides(Some(record)) || acting.id == record.id)
}

/// `UserRolePolicy#destroy?`.
fn can_destroy(acting: &Role, record: &Role) -> bool {
    !record.is_everyone()
        && acting.can(&[flag::MANAGE_ROLES])
        && acting.overrides(Some(record))
        && acting.id != record.id
}

async fn entities(state: &AppState, acting: &Role, rows: Vec<Row>) -> AppResult<Vec<AdminRole>> {
    let everyone = everyone_permissions(state).await?;
    let counts = sqlx::query!(
        r#"SELECT COALESCE(role_id, $1) AS "role_id!", count(*) AS "n!"
           FROM users GROUP BY COALESCE(role_id, $1)"#,
        EVERYONE_ROLE_ID,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let role = row.role(everyone);
            AdminRole {
                role: super::accounts::RoleEntity::of(&role),
                position: role.position,
                require_2fa: row.require_2fa,
                permissions_as_keys: flag::as_keys(row.permissions),
                everyone: role.is_everyone(),
                users_count: counts
                    .iter()
                    .find(|c| c.role_id == row.id)
                    .map_or(0, |c| c.n),
                can_update: can_update(acting, &role),
                can_destroy: can_destroy(acting, &role),
                created_at: super::super::convert::mastodon_date(row.created_at),
                updated_at: super::super::convert::mastodon_date(row.updated_at),
            }
        })
        .collect())
}

/// `UserRole.assignable`, lowest first, as entities for `acting`.
pub(super) async fn assignable(state: &AppState, acting: &Role) -> AppResult<Vec<AdminRole>> {
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id, name, color, position, permissions, highlighted, collection_limit,
                  require_2fa, created_at, updated_at
           FROM user_roles WHERE id <> $1 ORDER BY position ASC"#,
        EVERYONE_ROLE_ID
    )
    .fetch_all(&state.db)
    .await?;
    entities(state, acting, rows).await
}

async fn find(state: &AppState, id: i64) -> AppResult<Row> {
    sqlx::query_as!(
        Row,
        r#"SELECT id, name, color, position, permissions, highlighted, collection_limit,
                  require_2fa, created_at, updated_at
           FROM user_roles WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

async fn entity(state: &AppState, acting: &Role, id: i64) -> AppResult<AdminRole> {
    let row = find(state, id).await?;
    Ok(entities(state, acting, vec![row]).await?.remove(0))
}

/// `GET /api/v1/admin/roles/:id`: `Roles#edit`, which asks `update?`.
pub async fn get_admin_role(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminRole>> {
    auth.require_scope("admin:read")?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    let found = entity(&state, &acting, id).await?;
    role::authorize(found.can_update)?;
    Ok(Json(found))
}

#[derive(Debug, Deserialize)]
pub struct RoleForm {
    pub name: Option<String>,
    pub color: Option<String>,
    pub highlighted: Option<FlexBool>,
    pub position: Option<serde_json::Value>,
    pub require_2fa: Option<FlexBool>,
    pub collection_limit: Option<serde_json::Value>,
    pub permissions_as_keys: Option<Vec<String>>,
}

/// An integer attribute as Rails casts it, or the numericality error.
fn integer(value: &serde_json::Value) -> Result<i64, &'static str> {
    match value {
        serde_json::Value::Number(n) => n.as_i64().ok_or("must be an integer"),
        serde_json::Value::String(s) => {
            let s = s.trim();
            if let Ok(n) = s.parse::<i64>() {
                Ok(n)
            } else if s.parse::<f64>().is_ok() {
                Err("must be an integer")
            } else {
                Err("is not a number")
            }
        }
        _ => Err("is not a number"),
    }
}

/// What a save would write.
struct Draft {
    name: String,
    color: String,
    highlighted: bool,
    position: i32,
    require_2fa: bool,
    collection_limit: i32,
    permissions: i64,
}

/// `UserRole`'s validations with `current_account` set, as the controller
/// sets it, against `existing` when updating.
fn validate(form: RoleForm, existing: Option<&Row>, actor: &Role) -> Result<Draft, Vec<String>> {
    let mut errors = vec![];
    let everyone = existing.is_some_and(|r| r.id == EVERYONE_ROLE_ID);
    let name = form
        .name
        .unwrap_or_else(|| existing.map(|r| r.name.clone()).unwrap_or_default());
    let color = form
        .color
        .unwrap_or_else(|| existing.map(|r| r.color.clone()).unwrap_or_default());
    let mut position = existing.map_or(0, |r| i64::from(r.position));
    if let Some(value) = &form.position {
        match integer(value) {
            Ok(p) => position = p,
            Err(message) => errors.push(format!("Position {message}")),
        }
    }
    let mut collection_limit = existing.map_or(10, |r| i64::from(r.collection_limit));
    if let Some(value) = &form.collection_limit {
        match integer(value) {
            Ok(n) => collection_limit = n,
            Err(message) => errors.push(format!("Collection limit {message}")),
        }
    }
    let permissions = match &form.permissions_as_keys {
        Some(keys) => flag::from_keys(keys),
        None => existing.map_or(0, |r| r.permissions),
    };
    let require_2fa = form
        .require_2fa
        .map_or(existing.is_some_and(|r| r.require_2fa), |b| b.0);
    // `set_position`.
    if everyone {
        position = i64::from(NOBODY_POSITION);
    }

    if !everyone && name.trim().is_empty() {
        errors.push("Name can't be blank".into());
    }
    let css_color = regex::Regex::new(r"(?i)\A#?(?:[A-F0-9]{3}){1,2}\z").expect("valid regex");
    if !color.is_empty() && !css_color.is_match(&color) {
        errors.push("Color is invalid".into());
    }
    if !(-POSITION_LIMIT..=POSITION_LIMIT).contains(&position) {
        errors.push(format!(
            "Position must be in -{POSITION_LIMIT}..{POSITION_LIMIT}"
        ));
    }
    if collection_limit < 0 {
        errors.push("Collection limit must be greater than or equal to 0".into());
    }
    // `validate_permissions_elevation`.
    if actor.computed & permissions != permissions {
        errors.push(
            "Permissions as keys cannot include permissions your current role does not possess"
                .into(),
        );
    }
    // `validate_position_elevation`.
    if i64::from(actor.position) < position {
        errors.push("Position cannot be higher than your current role".into());
    }
    // `validate_dangerous_permissions`.
    if everyone && flag::SAFE & permissions != permissions {
        errors.push(
            "Permissions as keys include permissions that are not safe for the base role".into(),
        );
    }
    // `validate_own_role_edition`.
    if let Some(existing) = existing {
        if actor.id == Some(existing.id) {
            if permissions != existing.permissions {
                errors.push("Permissions as keys cannot be changed with your current role".into());
            }
            if position != i64::from(existing.position) && !everyone {
                errors.push("Position cannot be changed with your current role".into());
            }
            if require_2fa != existing.require_2fa && permissions & flag::ADMINISTRATOR == 0 {
                errors.push("Require 2fa cannot be changed with your current role".into());
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Draft {
        name,
        color,
        highlighted: form
            .highlighted
            .map_or(existing.is_some_and(|r| r.highlighted), |b| b.0),
        position: position as i32,
        require_2fa,
        collection_limit: collection_limit as i32,
        permissions,
    })
}

fn refuse(errors: Vec<String>) -> AppError {
    AppError::Unprocessable(format!("Validation failed: {}", errors.join(", ")))
}

/// The actor's own role, `current_account.user_role`, which the validations
/// compare against; the everyone role for a user without one.
async fn actor_role(state: &AppState, account_id: i64) -> AppResult<Role> {
    Ok(role::of_account(&state.db, account_id)
        .await?
        .unwrap_or_else(Role::nobody))
}

/// `POST /api/v1/admin/roles`: `Roles#create`.
pub async fn create_admin_role(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<RoleForm>,
) -> AppResult<Json<AdminRole>> {
    auth.require_scope("admin:write")?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    role::authorize(acting.can(&[flag::MANAGE_ROLES]))?;
    let actor = actor_role(&state, auth.account_id).await?;
    let draft = validate(form, None, &actor).map_err(refuse)?;
    let mut tx = state.db.begin().await?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO user_roles
             (name, color, highlighted, position, require_2fa, collection_limit,
              permissions, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
           RETURNING id"#,
        draft.name,
        draft.color,
        draft.highlighted,
        draft.position,
        draft.require_2fa,
        draft.collection_limit,
        draft.permissions,
    )
    .fetch_one(&mut *tx)
    .await?;
    action_log::log(
        &mut *tx,
        auth.account_id,
        "create",
        &Target::user_role(id, &draft.name),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(entity(&state, &acting, id).await?))
}

/// `PATCH /api/v1/admin/roles/:id`: `Roles#update`.
pub async fn update_admin_role(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<RoleForm>,
) -> AppResult<Json<AdminRole>> {
    auth.require_scope("admin:write")?;
    let existing = find(&state, id).await?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    let record = existing.role(everyone_permissions(&state).await?);
    role::authorize(can_update(&acting, &record))?;
    let actor = actor_role(&state, auth.account_id).await?;
    let draft = validate(form, Some(&existing), &actor).map_err(refuse)?;
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        r#"UPDATE user_roles
           SET name = $2, color = $3, highlighted = $4, position = $5, require_2fa = $6,
               collection_limit = $7, permissions = $8, updated_at = now()
           WHERE id = $1"#,
        id,
        draft.name,
        draft.color,
        draft.highlighted,
        draft.position,
        draft.require_2fa,
        draft.collection_limit,
        draft.permissions,
    )
    .execute(&mut *tx)
    .await?;
    action_log::log(
        &mut *tx,
        auth.account_id,
        "update",
        &Target::user_role(id, &draft.name),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(entity(&state, &acting, id).await?))
}

/// `DELETE /api/v1/admin/roles/:id`: `Roles#destroy`. Its users are left with
/// no role (`dependent: :nullify`), which is the everyone role.
pub async fn delete_admin_role(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("admin:write")?;
    let existing = find(&state, id).await?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    let record = existing.role(everyone_permissions(&state).await?);
    role::authorize(can_destroy(&acting, &record))?;
    let mut tx = state.db.begin().await?;
    sqlx::query!("UPDATE users SET role_id = NULL WHERE role_id = $1", id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM user_roles WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    action_log::log(
        &mut *tx,
        auth.account_id,
        "destroy",
        &Target::user_role(id, &existing.name),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(serde_json::json!({})))
}

#[cfg(test)]
mod tests {
    #[test]
    fn integers_cast_as_rails_casts_them() {
        assert_eq!(super::integer(&serde_json::json!("12")), Ok(12));
        assert_eq!(super::integer(&serde_json::json!(3)), Ok(3));
        assert_eq!(
            super::integer(&serde_json::json!("1.5")),
            Err("must be an integer")
        );
        assert_eq!(
            super::integer(&serde_json::json!("x")),
            Err("is not a number")
        );
    }
}
