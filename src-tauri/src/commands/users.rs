//! Users and roles: accounts, privileges and the changes from the forms.
//! Generated SQL is reviewed first with passwords masked; applying builds it
//! again from the same change. Only the executed statements carry the
//! password, and the history entry is the masked preview.

use std::time::Instant;

use dq_core::DbError;
use dq_core::adapter::Session;
use dq_core::sql::{ddl, users};
use dq_core::users::{AccountList, AccountPrivileges, AccountRef, UserAdmin, UserChange};
use tauri::State;

use super::query::record_history;
use super::{CmdResult, Guarded};
use crate::safety::{self, Confirmation, Intent};
use crate::state::AppState;

fn admin(session: &dyn Session) -> CmdResult<&dyn UserAdmin> {
    session.users().ok_or_else(|| {
        DbError::unsupported(
            "User and role management is available for PostgreSQL, CockroachDB, Greenplum, MySQL and MariaDB.",
        )
    })
}

#[tauri::command]
pub async fn user_accounts(state: State<'_, AppState>, session_id: String) -> CmdResult<AccountList> {
    let open = state.session(&session_id).await?;
    admin(&*open.session)?.accounts().await
}

/// Privileges of one account; schema and table grants are read from `database`.
#[tauri::command]
pub async fn user_privileges(
    state: State<'_, AppState>,
    session_id: String,
    account: AccountRef,
    database: String,
) -> CmdResult<AccountPrivileges> {
    let open = state.session(&session_id).await?;
    admin(&*open.session)?.privileges(&account, &database).await
}

/// SQL the change would run, passwords masked, for review.
#[tauri::command]
pub async fn user_change_preview(
    state: State<'_, AppState>,
    session_id: String,
    change: UserChange,
) -> CmdResult<String> {
    let open = state.session(&session_id).await?;
    let admin = admin(&*open.session)?;
    Ok(ddl::preview(&users::preview(open.session.dialect(), &admin.features(), &change)?))
}

/// Applies a reviewed change. Read-only profiles refuse; drops and revokes
/// confirm as destructive, and production asks for the connection name.
/// Returns the number of statements run.
#[tauri::command]
pub async fn apply_user_change(
    state: State<'_, AppState>,
    session_id: String,
    change: UserChange,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<u32>> {
    let open = state.session(&session_id).await?;
    let admin = admin(&*open.session)?;
    let (dialect, features) = (open.session.dialect(), admin.features());
    let shown = ddl::preview(&users::preview(dialect, &features, &change)?);
    let (connection, settings) = tokio::try_join!(state.connection(&open.connection_id), state.settings())?;
    let intent = Intent { writes: true, destructive: change.risks().into_iter().map(str::to_owned).collect() };
    if let Some(request) = safety::check(&connection, &settings, &intent, confirmation.as_ref())? {
        return Ok(Guarded::NeedsConfirmation { request });
    }
    let password = change.password().map(|p| p.expose().to_owned());
    // Prepared once: PostgreSQL's verifier has a random salt.
    let secret = password.as_deref().map(|p| admin.password_secret(p)).unwrap_or_default();
    let statements = users::statements(dialect, &features, &change, &|_| secret.clone())?;
    let started = Instant::now();
    let scrub = |e: DbError| match &password {
        Some(p) => e.without_secret(p).without_secret(&secret),
        None => e,
    };
    let result = admin.apply(change.database(), &statements).await.map_err(scrub);
    record_history(
        &state,
        &settings,
        &connection,
        &open,
        &shown,
        started.elapsed(),
        result.as_ref().map(|()| None).map_err(Clone::clone),
    );
    result?;
    Ok(Guarded::Done { value: statements.len() as u32 })
}
