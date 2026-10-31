use kelp_core::ClNumber;
use kelp_core::typemap::Typemap;
use redb::{ReadableTable, WriteTransaction};

use super::nodes::TxnNodes;
use super::{
    Applied, DEPOTS, GROUPS, LOCKS, MANIFESTS, META, NEXT_CL, PROTECTIONS, SETTINGS, SHELVES,
    STREAMS, TOKENS, TYPEMAPS, USERS, dec, enc, name_key, set_integration, stream_relative, submit,
};
use crate::acl::Protections;
use crate::record::{Depot, Group, Shelf, Stream, Token, TypemapRecord, User};
use crate::tree::fold;
use crate::{Command, Dirs, Error, Integration, Lock, LockDomain, Result};

/// How a command reaches the state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// Proposed on this server, which checks everything the command refers to.
    Local,
    /// Copied from the commit server's log. The command was checked there,
    /// and file manifests are fetched on demand rather than replicated, so
    /// their presence is not checked.
    Replicated,
}

pub(super) fn apply(txn: &WriteTransaction, cmd: &Command, origin: Origin) -> Result<Applied> {
    match cmd {
        Command::CreateUser {
            name,
            password_hash,
            admin,
            replica,
            at,
        } => {
            validate_name("user", name)?;
            if *replica && (*admin || password_hash.is_some()) {
                return Err(Error::Invalid(
                    "an edge account is a service account without administrative rights".into(),
                ));
            }
            let mut users = txn.open_table(USERS)?;
            let key = name_key(name);
            if users.get(key.as_str())?.is_some() {
                return Err(Error::AlreadyExists(format!("user {name}")));
            }
            if let Some(current) = super::accounts::alias_of(txn, &key)? {
                return Err(Error::AlreadyExists(format!(
                    "user {name}, a former name of {current}"
                )));
            }
            let user = User {
                name: name.clone(),
                password_hash: password_hash.clone(),
                admin: *admin,
                replica: *replica,
                totp: None,
                created_at: *at,
            };
            users.insert(key.as_str(), enc(&user).as_slice())?;
        }
        Command::SetPassword {
            name,
            password_hash,
        } => {
            let mut user = super::load_user(txn, name)?;
            if user.replica {
                return Err(Error::Invalid(format!(
                    "{name} is an edge account, which has no password"
                )));
            }
            if txn
                .open_table(super::SSO_LINKS)?
                .get(name_key(name).as_str())?
                .is_some()
            {
                return Err(Error::Invalid(format!(
                    "{name} logs in with single sign-on; unlink it first"
                )));
            }
            user.password_hash = Some(password_hash.clone());
            super::save_user(txn, &user)?;
        }
        Command::SetTotp { name, secret } => {
            let mut user = super::load_user(txn, name)?;
            if secret.is_some() && user.password_hash.is_none() {
                return Err(Error::Invalid(format!(
                    "{name} does not log in with a password"
                )));
            }
            user.totp = secret.clone();
            super::save_user(txn, &user)?;
        }
        Command::IssueToken {
            token_hash,
            user,
            expires_at,
        } => {
            let token = Token {
                user: user.clone(),
                expires_at: *expires_at,
            };
            txn.open_table(TOKENS)?
                .insert(token_hash.0.as_slice(), enc(&token).as_slice())?;
        }
        Command::RevokeToken { token_hash } => {
            txn.open_table(TOKENS)?.remove(token_hash.0.as_slice())?;
        }
        Command::SetGroup { name, members } => {
            validate_name("group", name)?;
            let users = txn.open_table(USERS)?;
            let mut names = Vec::with_capacity(members.len());
            for member in members {
                let user: User = match users.get(name_key(member).as_str())? {
                    Some(v) => dec("users", v.value())?,
                    None => return Err(Error::NotFound(format!("user {member}"))),
                };
                names.push(user.name);
            }
            names.sort();
            names.dedup();
            let mut groups = txn.open_table(GROUPS)?;
            let key = name_key(name);
            if names.is_empty() {
                groups.remove(key.as_str())?;
            } else {
                groups.insert(
                    key.as_str(),
                    enc(&Group {
                        name: name.clone(),
                        members: names,
                    })
                    .as_slice(),
                )?;
            }
        }
        Command::SetProtections { source } => {
            Protections::parse(source)?;
            txn.open_table(SETTINGS)?
                .insert(PROTECTIONS, source.as_bytes())?;
        }
        Command::CreateDepot { name, at } => {
            validate_name("depot", name)?;
            let mut depots = txn.open_table(DEPOTS)?;
            let key = name_key(name);
            if depots.get(key.as_str())?.is_some() {
                return Err(Error::AlreadyExists(format!("depot {name}")));
            }
            depots.insert(
                key.as_str(),
                enc(&Depot {
                    name: name.clone(),
                    created_at: *at,
                })
                .as_slice(),
            )?;
        }
        Command::SetTypemap {
            depot,
            source,
            by,
            at,
        } => {
            Typemap::parse(source).map_err(|e| Error::Invalid(e.to_string()))?;
            let key = name_key(depot);
            if txn.open_table(DEPOTS)?.get(key.as_str())?.is_none() {
                return Err(Error::NotFound(format!("depot {depot}")));
            }
            let record = TypemapRecord {
                source: source.clone(),
                set_by: by.clone(),
                set_at: *at,
            };
            txn.open_table(TYPEMAPS)?
                .insert(key.as_str(), enc(&record).as_slice())?;
        }
        Command::CreateStream {
            path,
            kind,
            parent,
            at,
        } => {
            let segments: Vec<_> = path.segments().collect();
            if segments.len() != 2 {
                return Err(Error::Invalid(format!(
                    "stream path {path} must be //depot/stream"
                )));
            }
            if txn
                .open_table(DEPOTS)?
                .get(name_key(segments[0]).as_str())?
                .is_none()
            {
                return Err(Error::NotFound(format!("depot {}", segments[0])));
            }
            if kind.has_parent() != parent.is_some() {
                return Err(Error::Invalid(format!(
                    "a {kind:?} stream {} a parent",
                    match parent {
                        Some(_) => "cannot have",
                        None => "needs",
                    }
                )));
            }
            let mut streams = txn.open_table(STREAMS)?;
            if streams.get(path.key())?.is_some() {
                return Err(Error::AlreadyExists(format!("stream {path}")));
            }
            // A deleted stream's history stays under its path.
            let used = txn
                .open_table(super::STREAM_CHANGES)?
                .range((path.key(), 0u64)..=(path.key(), u64::MAX))?
                .next()
                .is_some();
            if used {
                return Err(Error::AlreadyExists(format!(
                    "stream {path} existed before and keeps its history; choose another name"
                )));
            }
            let parent: Option<Stream> = match parent {
                Some(p) if p.depot().to_lowercase() != segments[0].to_lowercase() => {
                    return Err(Error::Invalid(format!("{p} is in another depot")));
                }
                Some(p) => match streams.get(p.key())? {
                    Some(v) => Some(dec("streams", v.value())?),
                    None => return Err(Error::NotFound(format!("stream {p}"))),
                },
                None => None,
            };
            let root = match &parent {
                Some(p) => p.root,
                None => Dirs::new(&TxnNodes::new(txn)?).empty()?,
            };
            let lock_domain = match &parent {
                Some(p) if kind.shares_parent_locks() => p.lock_domain.clone(),
                _ => LockDomain(path.key().to_owned()),
            };
            let stream = Stream {
                path: path.clone(),
                kind: *kind,
                parent: parent.as_ref().map(|p| p.path.clone()),
                head: ClNumber(0),
                root,
                base_root: root,
                lock_domain,
                created_from: parent.as_ref().map_or(ClNumber(0), |p| p.head),
                created_at: *at,
            };
            streams.insert(path.key(), enc(&stream).as_slice())?;
            drop(streams);
            if let Some(p) = parent {
                set_integration(
                    txn,
                    path,
                    &Integration {
                        source: p.path,
                        through: p.head,
                    },
                )?;
            }
        }
        Command::Submit { cl, submission } => {
            return submit::apply_submit(txn, *cl, submission, origin);
        }
        Command::Shelve { cl, contents } => {
            if contents.files.is_empty() {
                return Err(Error::Invalid("a shelf needs at least one file".into()));
            }
            if txn
                .open_table(STREAMS)?
                .get(contents.stream.key())?
                .is_none()
            {
                return Err(Error::NotFound(format!("stream {}", contents.stream)));
            }
            let manifests = txn.open_table(MANIFESTS)?;
            let mut missing = Vec::new();
            for file in &contents.files {
                if stream_relative(&contents.stream, &file.path).is_none() {
                    return Err(Error::Invalid(format!(
                        "{} is not in stream {}",
                        file.path, contents.stream
                    )));
                }
                if let Some(data) = file.action.data()
                    && origin == Origin::Local
                    && manifests.get(data.content.0.0.as_slice())?.is_none()
                {
                    missing.push(file.path.to_string());
                }
            }
            if !missing.is_empty() {
                return Err(Error::MissingContent(missing));
            }
            let mut meta = txn.open_table(META)?;
            let next = meta.get(NEXT_CL)?.map_or(1, |v| v.value());
            let mut shelves = txn.open_table(SHELVES)?;
            if cl.0 == next {
                meta.insert(NEXT_CL, next + 1)?;
            } else {
                let existing: Shelf = match shelves.get(cl.0)? {
                    Some(v) => dec("shelves", v.value())?,
                    None => return Err(Error::NotFound(format!("shelf {cl}"))),
                };
                // A review follows its shelf, so the shelf keeps its stream
                // and owner.
                if existing.contents.stream != contents.stream
                    || name_key(&existing.contents.owner) != name_key(&contents.owner)
                {
                    return Err(Error::Invalid(format!(
                        "shelf {cl} belongs to {} in {}",
                        existing.contents.owner, existing.contents.stream
                    )));
                }
            }
            let shelf = Shelf {
                number: *cl,
                contents: contents.clone(),
            };
            shelves.insert(cl.0, enc(&shelf).as_slice())?;
            drop((shelves, meta, manifests));
            super::governance::reviews::shelved(txn, &shelf)?;
        }
        Command::DeleteShelf { cl } => {
            if txn.open_table(SHELVES)?.remove(cl.0)?.is_none() {
                return Err(Error::NotFound(format!("shelf {cl}")));
            }
            super::governance::reviews::shelf_deleted(txn, *cl)?;
        }
        Command::Governance(g) => super::governance::apply(txn, g, origin)?,
        Command::Obliterate { paths } => super::obliterate::apply(txn, paths)?,
        Command::SetImports { stream, imports } => super::imports::set(txn, stream, imports)?,
        Command::ExpectSso { user, sso_name } => {
            super::accounts::expect_sso(txn, user, sso_name.as_deref())?
        }
        Command::LinkSso {
            user,
            issuer,
            subject,
        } => super::accounts::link_sso(txn, user, issuer, subject)?,
        Command::UnlinkSso { user } => super::accounts::unlink_sso(txn, user)?,
        Command::MergeUser { from, into } => super::accounts::merge(txn, from, into)?,
        Command::DeleteStream { path } => super::streams::delete(txn, path)?,
        Command::RecordMoves { cl, moves } => super::moves::record(txn, *cl, moves)?,
        Command::SetLabel {
            name,
            target,
            description,
            by,
            at,
        } => super::labels::set(txn, name, target.as_ref(), description, by, *at)?,
        Command::SetDisabled { user, disabled } => {
            super::accounts::set_disabled(txn, user, *disabled)?
        }
        Command::EditProfile { user, edit } => super::accounts::edit_profile(txn, user, edit)?,
        Command::SetMembership {
            group,
            user,
            member,
        } => super::accounts::set_membership(txn, group, user, *member)?,
        Command::PublishArtifact { id, contents } => {
            super::artifacts::publish(txn, *id, contents, origin)?
        }
        Command::PinArtifact { id, pinned } => super::artifacts::pin(txn, *id, *pinned)?,
        Command::DeleteArtifact { id } => super::artifacts::delete(txn, *id)?,
        Command::SetArtifactRetention {
            stream,
            name,
            keep_last,
        } => super::artifacts::set_retention(txn, stream, name, *keep_last)?,
        Command::AcquireLocks {
            holder,
            domain,
            paths,
        } => {
            let mut locks = txn.open_table(LOCKS)?;
            let mut conflicts = Vec::new();
            for path in paths {
                if let Some(v) = locks.get((domain.0.as_str(), fold(path).as_str()))? {
                    let lock: Lock = dec("locks", v.value())?;
                    if lock.holder != *holder {
                        conflicts.push(format!(
                            "{path} (locked by {}@{})",
                            lock.holder.user, lock.holder.workspace
                        ));
                    }
                }
            }
            if !conflicts.is_empty() {
                return Err(Error::Locked(conflicts));
            }
            for path in paths {
                let lock = Lock {
                    path: path.clone(),
                    holder: holder.clone(),
                };
                locks.insert(
                    (domain.0.as_str(), fold(path).as_str()),
                    enc(&lock).as_slice(),
                )?;
            }
        }
        Command::ReleaseLocks {
            holder,
            domain,
            paths,
        } => {
            let mut locks = txn.open_table(LOCKS)?;
            for path in paths {
                let key = fold(path);
                let held_by_other = match locks.get((domain.0.as_str(), key.as_str()))? {
                    Some(v) => holder.as_ref().is_some_and(|h| {
                        dec::<Lock>("locks", v.value()).is_ok_and(|l| l.holder != *h)
                    }),
                    None => true,
                };
                if !held_by_other {
                    locks.remove((domain.0.as_str(), key.as_str()))?;
                }
            }
        }
    }
    Ok(Applied::Done)
}

pub(super) fn validate_name(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.');
    ok.then_some(())
        .ok_or_else(|| Error::Invalid(format!("invalid {kind} name {name:?}")))
}
