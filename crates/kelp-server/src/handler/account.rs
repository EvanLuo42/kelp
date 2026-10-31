use std::sync::Arc;

use kelp_meta::Command;
use kelp_proto::rpc::GroupInfo;
use kelp_proto::{Auth, ErrorKind, ErrorReply, Reply, Request};

use kelp_meta::record::User;

use super::{Access, Response, Session, blocking, internal, meta_error, not_found};
use crate::edge::upstream_error;
use crate::role::Forwarder;
use crate::{State, auth};

const MAX_TOKEN_DAYS: u32 = 3650;

pub(super) async fn login(
    state: &Arc<State>,
    session: &Session,
    name: String,
    password: String,
    code: Option<String>,
) -> Response {
    let user = blocking({
        let state = state.clone();
        move || {
            let reader = state.reader()?;
            let user = match reader.user(&name).map_err(meta_error)? {
                Some(user) => Some(user),
                None => match reader.alias(&name).map_err(meta_error)? {
                    Some(current) => reader.user(&current).map_err(meta_error)?,
                    None => None,
                },
            };
            let wrong =
                || ErrorReply::new(ErrorKind::Unauthenticated, "wrong user name or password");
            let user = user.ok_or_else(wrong)?;
            if user.password_hash.is_none()
                && reader.sso_link(&user.name).map_err(meta_error)?.is_some()
            {
                return Err(ErrorReply::new(
                    ErrorKind::Unauthenticated,
                    format!(
                        "{} logs in with single sign-on; run `kelp login <server> --sso`",
                        user.name
                    ),
                ));
            }
            let verified = user
                .password_hash
                .as_deref()
                .is_some_and(|h| auth::verify_password(&password, h));
            if !verified {
                return Err(wrong());
            }
            if reader.is_disabled(&user.name).map_err(meta_error)? {
                return Err(super::disabled(&user.name));
            }
            let required = state.oidc.as_ref().is_some_and(|o| o.config.require);
            if required && !user.admin {
                return Err(ErrorReply::new(
                    ErrorKind::Unauthenticated,
                    "this server requires single sign-on; run `kelp login <server> --sso`",
                ));
            }
            Ok(user)
        }
    })
    .await?;
    if let Some(secret) = &user.totp {
        let code = code.ok_or_else(|| {
            ErrorReply::new(
                ErrorKind::CodeRequired,
                "enter the code from your authenticator app",
            )
        })?;
        check_code(state, &user.name, secret, &code)?;
    }
    start_session(state, session, user).await
}

/// Checks a TOTP code, refusing one that was already used.
fn check_code(state: &State, user: &str, secret: &str, code: &str) -> Result<(), ErrorReply> {
    let wrong = || ErrorReply::new(ErrorKind::Unauthenticated, "wrong or reused code");
    let step = auth::check_totp(secret, code, state.clock.now()).ok_or_else(wrong)?;
    let mut used = state.totp_steps.lock().expect("totp lock");
    let last = used.entry(user.to_lowercase()).or_insert(0);
    if step <= *last {
        return Err(wrong());
    }
    *last = step;
    Ok(())
}

/// Issues a token for `user` and logs the session in as them.
async fn start_session(state: &State, session: &Session, user: User) -> Response {
    let (token, expires_at) = issue(
        state,
        &user.name,
        &user.name,
        state.token_ttl.as_millis() as u64,
    )
    .await?;
    let reply = Reply::Token {
        token,
        user: user.name.clone(),
        expires_at,
    };
    session.set_user(Some(user));
    Ok(reply)
}

pub(super) async fn oidc_start(state: &State) -> Response {
    let oidc = oidc(state)?;
    Ok(Reply::DeviceCode(oidc.start().await?))
}

pub(super) async fn oidc_poll(
    state: &Arc<State>,
    session: &Session,
    device_code: String,
) -> Response {
    let oidc = oidc(state)?;
    let Some(identity) = oidc.poll(&device_code, state.clock.now()).await? else {
        return Ok(Reply::AuthPending);
    };
    let user = sso_user(state, oidc, &identity).await?;
    sync_groups(state, oidc, &user, &identity).await?;
    start_session(state, session, user).await
}

/// Links the single sign-on identity a device login approves to the
/// logged-in user and starts a new session, since linking revokes the
/// user's tokens.
pub(super) async fn oidc_link(
    state: &Arc<State>,
    session: &Session,
    access: &Access,
    device_code: String,
) -> Response {
    let oidc = oidc(state)?;
    let Some(identity) = oidc.poll(&device_code, state.clock.now()).await? else {
        return Ok(Reply::AuthPending);
    };
    let cmd = Command::LinkSso {
        user: access.name().to_owned(),
        issuer: identity.issuer.clone(),
        subject: identity.subject.clone(),
    };
    state.commit(access.name(), cmd).await?;
    let user = load_user(state, access.name())?;
    sync_groups(state, oidc, &user, &identity).await?;
    start_session(state, session, user).await
}

/// Returns the user a single sign-on identity logs in as, linking or
/// creating it on the first sign-on.
///
/// An identity logs in as the user it is linked to. A first sign-on links
/// to the user an administrator expects it for, or creates a user of its
/// name. It never takes over an existing user of the same name, since a
/// provider's user names can change hands; that user links itself with
/// `kelp login --sso --link`.
async fn sso_user(
    state: &State,
    oidc: &crate::oidc::Oidc,
    identity: &crate::oidc::Identity,
) -> Result<User, ErrorReply> {
    let (linked, expected, taken) = {
        let reader = state.reader()?;
        let linked = reader
            .sso_user(&identity.issuer, &identity.subject)
            .map_err(meta_error)?;
        let expected = reader.expected_sso(&identity.user).map_err(meta_error)?;
        let taken = reader.user(&identity.user).map_err(meta_error)?.is_some()
            || reader.alias(&identity.user).map_err(meta_error)?.is_some();
        (linked, expected, taken)
    };
    if let Some(name) = linked {
        return load_user(state, &name);
    }
    let name = match expected {
        Some(name) => name,
        None if taken => {
            let name = &identity.user;
            return Err(ErrorReply::new(
                ErrorKind::Unauthenticated,
                format!(
                    "Kelp already has a user {name} that is not linked to this single sign-on account; log in as {name} with its password and run `kelp login <server> --sso --link`, or ask an administrator to run `kelp user link {name} --sso {name}`"
                ),
            ));
        }
        None if oidc.config.auto_create => {
            // Creating and linking happen together, or a failure between
            // them would leave a user that every later sign-on refuses.
            let create = Command::CreateUser {
                name: identity.user.clone(),
                password_hash: None,
                admin: false,
                replica: false,
                at: state.clock.now(),
            };
            let link = Command::LinkSso {
                user: identity.user.clone(),
                issuer: identity.issuer.clone(),
                subject: identity.subject.clone(),
            };
            state.commit_all("oidc", vec![create, link]).await?;
            return load_user(state, &identity.user);
        }
        None => {
            return Err(ErrorReply::new(
                ErrorKind::Unauthenticated,
                format!("there is no Kelp user {}", identity.user),
            ));
        }
    };
    let cmd = Command::LinkSso {
        user: name.clone(),
        issuer: identity.issuer.clone(),
        subject: identity.subject.clone(),
    };
    state.commit("oidc", cmd).await?;
    load_user(state, &name)
}

/// Adds the user to the groups the provider lists, and removes them from
/// managed groups it no longer lists.
async fn sync_groups(
    state: &State,
    oidc: &crate::oidc::Oidc,
    user: &User,
    identity: &crate::oidc::Identity,
) -> Result<(), ErrorReply> {
    if oidc.config.groups_claim.is_none() {
        return Ok(());
    }
    let wanted: Vec<String> = identity.groups.iter().map(|g| g.to_lowercase()).collect();
    let managed: Vec<String> = oidc
        .config
        .managed_groups
        .iter()
        .map(|g| g.to_lowercase())
        .collect();
    let groups = state.reader()?.groups().map_err(meta_error)?;
    let mut changes = Vec::new();
    for group in groups {
        let key = group.name.to_lowercase();
        let member = group
            .members
            .iter()
            .any(|m| m.eq_ignore_ascii_case(&user.name));
        let wanted = wanted.contains(&key);
        if member != wanted && (wanted || managed.contains(&key)) {
            changes.push(Command::SetMembership {
                group: group.name,
                user: user.name.clone(),
                member: wanted,
            });
        }
    }
    if !changes.is_empty() {
        state.commit_all("oidc", changes).await?;
    }
    Ok(())
}

fn load_user(state: &State, name: &str) -> Result<User, ErrorReply> {
    let user = state
        .reader()?
        .user(name)
        .map_err(meta_error)?
        .ok_or_else(|| internal(format!("user {name} vanished")))?;
    if user.replica {
        return Err(ErrorReply::new(
            ErrorKind::Unauthenticated,
            "edge accounts cannot use single sign-on",
        ));
    }
    if state
        .reader()?
        .is_disabled(&user.name)
        .map_err(meta_error)?
    {
        return Err(super::disabled(&user.name));
    }
    Ok(user)
}

pub(super) async fn expect_sso(
    state: &State,
    access: &Access,
    user: String,
    sso_name: Option<String>,
) -> Response {
    access.require_admin()?;
    state
        .commit(access.name(), Command::ExpectSso { user, sso_name })
        .await?;
    Ok(Reply::Ok)
}

pub(super) async fn unlink_sso(state: &State, access: &Access, user: String) -> Response {
    access.require_admin()?;
    state
        .commit(access.name(), Command::UnlinkSso { user })
        .await?;
    Ok(Reply::Ok)
}

pub(super) async fn merge_user(
    state: &State,
    access: &Access,
    from: String,
    into: String,
) -> Response {
    access.require_admin()?;
    state
        .commit(access.name(), Command::MergeUser { from, into })
        .await?;
    Ok(Reply::Ok)
}

pub(super) fn get_user(state: &State, access: &Access, name: String) -> Response {
    access.require_admin()?;
    let reader = state.reader()?;
    let current = match reader.alias(&name).map_err(meta_error)? {
        Some(current) => current,
        None => name.clone(),
    };
    let user = reader
        .user(&current)
        .map_err(meta_error)?
        .ok_or_else(|| not_found(format!("user {name}")))?;
    Ok(Reply::User(user_info(&reader, user)?))
}

pub(super) fn list_users(state: &State, access: &Access) -> Response {
    access.require_admin()?;
    let reader = state.reader()?;
    let users = reader.users().map_err(meta_error)?;
    let infos = users
        .into_iter()
        .map(|u| user_info(&reader, u))
        .collect::<Result<_, _>>()?;
    Ok(Reply::Users(infos))
}

fn user_info(
    reader: &kelp_meta::db::Reader,
    user: User,
) -> Result<kelp_proto::rpc::UserInfo, ErrorReply> {
    let sso = reader
        .sso_link(&user.name)
        .map_err(meta_error)?
        .map(|l| format!("{}@{}", l.subject, l.issuer));
    Ok(kelp_proto::rpc::UserInfo {
        admin: user.admin,
        password: user.password_hash.is_some(),
        replica: user.replica,
        sso,
        expected_sso: reader.expected_sso_of(&user.name).map_err(meta_error)?,
        aliases: reader.aliases_of(&user.name).map_err(meta_error)?,
        disabled: reader.is_disabled(&user.name).map_err(meta_error)?,
        name: user.name,
    })
}

/// Changes a password. Users change their own with the current one;
/// administrators set anyone else's.
pub(super) async fn set_password(
    state: &State,
    access: &Access,
    user: Option<String>,
    current: Option<String>,
    password: String,
) -> Response {
    let target = user.unwrap_or_else(|| access.name().to_owned());
    let own = target.eq_ignore_ascii_case(access.name());
    if !own {
        access.require_admin()?;
    }
    if password.len() < 8 {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            "a password needs at least 8 characters",
        ));
    }
    if own {
        let hash = access.user.password_hash.clone().ok_or_else(|| {
            ErrorReply::new(ErrorKind::Invalid, format!("{target} has no password"))
        })?;
        let current = current.unwrap_or_default();
        let ok = blocking(move || Ok(auth::verify_password(&current, &hash))).await?;
        if !ok {
            return Err(ErrorReply::new(
                ErrorKind::Unauthenticated,
                "the current password is wrong",
            ));
        }
    }
    let password_hash = blocking(move || Ok(auth::hash_password(&password))).await?;
    let cmd = Command::SetPassword {
        name: target,
        password_hash,
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

/// Revokes the token the session logged in with.
pub(super) async fn logout(state: &State, session: &Session, access: &Access) -> Response {
    let token = session.token.lock().expect("session lock").clone();
    let token = token.ok_or_else(|| {
        ErrorReply::new(
            ErrorKind::Invalid,
            "this connection did not log in with a token",
        )
    })?;
    let cmd = Command::RevokeToken {
        token_hash: auth::token_hash(&token),
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) async fn set_disabled(
    state: &State,
    access: &Access,
    user: String,
    disabled: bool,
) -> Response {
    access.require_admin()?;
    if disabled && user.eq_ignore_ascii_case(access.name()) {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            "administrators cannot disable themselves",
        ));
    }
    state
        .commit(access.name(), Command::SetDisabled { user, disabled })
        .await?;
    Ok(Reply::Ok)
}

fn oidc(state: &State) -> Result<&crate::oidc::Oidc, ErrorReply> {
    state
        .oidc
        .as_ref()
        .ok_or_else(|| ErrorReply::new(ErrorKind::Invalid, "this server has no single sign-on"))
}

pub(super) async fn totp_enroll(access: &Access) -> Response {
    if access.user.password_hash.is_none() {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            "only users who log in with a password can add a second factor",
        ));
    }
    let secret = auth::new_totp_secret();
    let uri = format!(
        "otpauth://totp/Kelp:{}?secret={secret}&issuer=Kelp&digits=6&period=30",
        access.name()
    );
    Ok(Reply::TotpSecret { secret, uri })
}

pub(super) async fn totp_confirm(
    state: &State,
    access: &Access,
    secret: String,
    code: String,
) -> Response {
    if auth::check_totp(&secret, &code, state.clock.now()).is_none() {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            "the code does not match; check the authenticator app's clock",
        ));
    }
    let cmd = Command::SetTotp {
        name: access.name().to_owned(),
        secret: Some(secret),
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) async fn totp_disable(
    state: &State,
    access: &Access,
    user: Option<String>,
    code: Option<String>,
) -> Response {
    let name = user.unwrap_or_else(|| access.name().to_owned());
    if !name.eq_ignore_ascii_case(access.name()) || code.is_none() {
        access.require_admin()?;
    } else if let (Some(secret), Some(code)) = (&access.user.totp, &code) {
        check_code(state, &name, secret, code)?;
    }
    let cmd = Command::SetTotp { name, secret: None };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

/// Logs in on an edge by forwarding a login request to the commit server,
/// which issues the token.
pub(super) async fn login_through(
    state: &State,
    to: Forwarder<'_>,
    session: &Session,
    request: Request,
) -> Response {
    let conn = to.connect(Auth::None).await?;
    let reply = conn.call(&request).await.map_err(upstream_error)?;
    let Reply::Token { token, user, .. } = &reply else {
        return Ok(reply);
    };
    to.catch_up().await?;
    let record = state.reader()?.user(user.as_str()).map_err(meta_error)?;
    let record = record.ok_or_else(|| internal(format!("user {user} was not replicated")))?;
    session.set_user(Some(record));
    session.set_token(Some(token.clone()));
    *session.upstream.lock().await = Some(conn);
    Ok(reply)
}

pub(super) async fn create_user(
    state: &Arc<State>,
    access: &Access,
    name: String,
    password: Option<String>,
    admin: bool,
    replica: bool,
) -> Response {
    access.require_admin()?;
    let password_hash = blocking(move || Ok(password.as_deref().map(auth::hash_password))).await?;
    let cmd = Command::CreateUser {
        name,
        password_hash,
        admin,
        replica,
        at: state.clock.now(),
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) async fn issue_token(
    state: &State,
    access: &Access,
    user: String,
    ttl_days: u32,
) -> Response {
    access.require_admin()?;
    if !(1..=MAX_TOKEN_DAYS).contains(&ttl_days) {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            format!("ttl must be 1 to {MAX_TOKEN_DAYS} days"),
        ));
    }
    let record = state.reader()?.user(&user).map_err(meta_error)?;
    let user = record.ok_or_else(|| not_found(format!("user {user}")))?;
    let (token, expires_at) = issue(
        state,
        access.name(),
        &user.name,
        u64::from(ttl_days) * 24 * 3600 * 1000,
    )
    .await?;
    Ok(Reply::Token {
        token,
        user: user.name,
        expires_at,
    })
}

async fn issue(
    state: &State,
    actor: &str,
    user: &str,
    ttl_millis: u64,
) -> Result<(String, u64), ErrorReply> {
    let (token, token_hash) = auth::new_token();
    let expires_at = state.clock.now() + ttl_millis;
    let cmd = Command::IssueToken {
        token_hash,
        user: user.to_owned(),
        expires_at,
    };
    state.commit(actor, cmd).await?;
    Ok((token, expires_at))
}

pub(super) async fn set_group(
    state: &State,
    access: &Access,
    name: String,
    members: Vec<String>,
) -> Response {
    access.require_admin()?;
    state
        .commit(access.name(), Command::SetGroup { name, members })
        .await?;
    Ok(Reply::Ok)
}

pub(super) fn list_groups(state: &State) -> Response {
    let groups = state.reader()?.groups().map_err(meta_error)?;
    Ok(Reply::Groups(
        groups
            .into_iter()
            .map(|g| GroupInfo {
                name: g.name,
                members: g.members,
            })
            .collect(),
    ))
}

pub(super) fn get_protections(state: &State, access: &Access) -> Response {
    access.require_admin()?;
    Ok(Reply::Text(
        state.reader()?.protections_source().map_err(meta_error)?,
    ))
}

pub(super) async fn set_protections(state: &State, access: &Access, source: String) -> Response {
    access.require_admin()?;
    state
        .commit(access.name(), Command::SetProtections { source })
        .await?;
    Ok(Reply::Ok)
}
