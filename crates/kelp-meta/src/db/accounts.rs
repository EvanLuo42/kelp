//! Links between users and single sign-on identities, and merged users.

use redb::{ReadableTable, WriteTransaction};

use super::{
    ALIASES, AUDIT, DISABLED, EMAIL_OWNERS, GROUPS, LOCKS, LOG, PROFILES, PROTECTIONS,
    REVIEW_RULES, REVIEWS, Reader, SETTINGS, SHELVES, SSO_EXPECTED, SSO_LINKS, SSO_SUBJECTS,
    TOKENS, USERS, dec, enc, load_user, name_key, save_user,
};
use crate::lock::Lock;
use crate::record::{AuditEntry, Group, Profile, Review, Shelf, Token, User};
use crate::{Command, Error, ProfileEdit, Result};

/// The most addresses a user may have.
const MAX_EMAILS: usize = 16;

/// A single sign-on identity: the provider's issuer URL and the subject it
/// identifies the user by, which unlike a user name never changes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SsoIdentity {
    pub issuer: String,
    pub subject: String,
}

/// Returns the user that `key`, a folded name, used to belong to.
pub(super) fn alias_of(txn: &WriteTransaction, key: &str) -> Result<Option<String>> {
    Ok(txn
        .open_table(ALIASES)?
        .get(key)?
        .map(|v| v.value().to_owned()))
}

fn no_edge_account(user: &User) -> Result<()> {
    match user.replica {
        true => Err(Error::Invalid(format!(
            "{} is an edge account, which has no single sign-on or other names",
            user.name
        ))),
        false => Ok(()),
    }
}

pub(super) fn expect_sso(txn: &WriteTransaction, user: &str, sso_name: Option<&str>) -> Result<()> {
    let user = load_user(txn, user)?;
    no_edge_account(&user)?;
    let mut expected = txn.open_table(SSO_EXPECTED)?;
    expected.retain(|_, owner| !owner.eq_ignore_ascii_case(&user.name))?;
    if let Some(sso_name) = sso_name {
        let key = name_key(sso_name);
        if let Some(other) = expected.get(key.as_str())? {
            return Err(Error::AlreadyExists(format!(
                "single sign-on as {sso_name} is already expected for {}",
                other.value()
            )));
        }
        expected.insert(key.as_str(), user.name.as_str())?;
    }
    Ok(())
}

pub(super) fn link_sso(
    txn: &WriteTransaction,
    user: &str,
    issuer: &str,
    subject: &str,
) -> Result<()> {
    if issuer.is_empty() || subject.is_empty() {
        return Err(Error::Invalid(
            "a single sign-on identity needs an issuer and a subject".into(),
        ));
    }
    let mut user = load_user(txn, user)?;
    no_edge_account(&user)?;
    let identity = SsoIdentity {
        issuer: issuer.to_owned(),
        subject: subject.to_owned(),
    };
    let mut subjects = txn.open_table(SSO_SUBJECTS)?;
    if let Some(owner) = subjects.get((issuer, subject))? {
        let owner = owner.value().to_owned();
        if !owner.eq_ignore_ascii_case(&user.name) {
            return Err(Error::AlreadyExists(format!(
                "that single sign-on identity is linked to {owner}"
            )));
        }
    }
    let mut links = txn.open_table(SSO_LINKS)?;
    let key = name_key(&user.name);
    if let Some(existing) = links.get(key.as_str())? {
        let existing: SsoIdentity = dec("sso_links", existing.value())?;
        if existing != identity {
            return Err(Error::Invalid(format!(
                "{} is linked to another single sign-on identity; unlink it first",
                user.name
            )));
        }
        // Already linked: nothing changes, and sessions just started stay.
        return Ok(());
    }
    subjects.insert((issuer, subject), user.name.as_str())?;
    links.insert(key.as_str(), enc(&identity).as_slice())?;
    drop((subjects, links));
    txn.open_table(SSO_EXPECTED)?
        .retain(|_, owner| !owner.eq_ignore_ascii_case(&user.name))?;
    user.password_hash = None;
    user.totp = None;
    save_user(txn, &user)?;
    txn.open_table(TOKENS)?.retain(|_, v| {
        postcard::from_bytes::<Token>(v).map_or(true, |t| !t.user.eq_ignore_ascii_case(&user.name))
    })?;
    Ok(())
}

pub(super) fn unlink_sso(txn: &WriteTransaction, user: &str) -> Result<()> {
    let user = load_user(txn, user)?;
    let key = name_key(&user.name);
    let link = txn
        .open_table(SSO_LINKS)?
        .remove(key.as_str())?
        .map(|v| v.value().to_vec());
    if let Some(link) = link {
        let link: SsoIdentity = dec("sso_links", &link)?;
        txn.open_table(SSO_SUBJECTS)?
            .remove((link.issuer.as_str(), link.subject.as_str()))?;
    }
    txn.open_table(SSO_EXPECTED)?
        .retain(|_, owner| !owner.eq_ignore_ascii_case(&user.name))?;
    Ok(())
}

pub(super) fn set_disabled(txn: &WriteTransaction, user: &str, disabled: bool) -> Result<()> {
    let user = load_user(txn, user)?;
    let key = name_key(&user.name);
    let mut table = txn.open_table(DISABLED)?;
    if !disabled {
        table.remove(key.as_str())?;
        return Ok(());
    }
    table.insert(key.as_str(), ())?;
    drop(table);
    txn.open_table(TOKENS)?.retain(|_, v| {
        postcard::from_bytes::<Token>(v).map_or(true, |t| !t.user.eq_ignore_ascii_case(&user.name))
    })?;
    Ok(())
}

fn valid_display_name(name: &str) -> Result<String> {
    let name = name.trim();
    let ok = !name.is_empty() && name.chars().count() <= 64 && !name.chars().any(char::is_control);
    ok.then(|| name.to_owned())
        .ok_or_else(|| Error::Invalid(format!("invalid display name {name:?}")))
}

fn valid_email(email: &str) -> Result<String> {
    let email = email.trim();
    let ok = email.len() <= 254
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        });
    ok.then(|| email.to_owned())
        .ok_or_else(|| Error::Invalid(format!("invalid email address {email:?}")))
}

pub(super) fn edit_profile(txn: &WriteTransaction, user: &str, edit: &ProfileEdit) -> Result<()> {
    let user = load_user(txn, user)?;
    let key = name_key(&user.name);
    let mut profiles = txn.open_table(PROFILES)?;
    let mut profile: Profile = match profiles.get(key.as_str())? {
        Some(v) => dec("profiles", v.value())?,
        None => Profile::default(),
    };
    let mut owners = txn.open_table(EMAIL_OWNERS)?;
    match edit {
        ProfileEdit::DisplayName(name) => {
            profile.display_name = name.as_deref().map(valid_display_name).transpose()?;
        }
        ProfileEdit::AddEmail(email) => {
            let email = valid_email(email)?;
            let email_key = email.to_lowercase();
            if let Some(owner) = owners.get(email_key.as_str())? {
                let owner = owner.value();
                return Err(Error::AlreadyExists(
                    match owner.eq_ignore_ascii_case(&user.name) {
                        true => format!("{email} is already an address of {owner}"),
                        false => format!("the address {email} belongs to {owner}"),
                    },
                ));
            }
            if profile.emails.len() >= MAX_EMAILS {
                return Err(Error::Invalid(format!(
                    "{} already has {MAX_EMAILS} addresses",
                    user.name
                )));
            }
            owners.insert(email_key.as_str(), user.name.as_str())?;
            profile.emails.push(email);
        }
        ProfileEdit::RemoveEmail(email) => {
            let email_key = email.trim().to_lowercase();
            let before = profile.emails.len();
            profile.emails.retain(|e| e.to_lowercase() != email_key);
            if profile.emails.len() == before {
                return Err(Error::NotFound(format!("address {email} of {}", user.name)));
            }
            owners.remove(email_key.as_str())?;
        }
        ProfileEdit::PrimaryEmail(email) => {
            let email_key = email.trim().to_lowercase();
            let Some(at) = profile
                .emails
                .iter()
                .position(|e| e.to_lowercase() == email_key)
            else {
                return Err(Error::NotFound(format!("address {email} of {}", user.name)));
            };
            let primary = profile.emails.remove(at);
            profile.emails.insert(0, primary);
        }
    }
    drop(owners);
    match profile == Profile::default() {
        true => profiles.remove(key.as_str())?,
        false => profiles.insert(key.as_str(), enc(&profile).as_slice())?,
    };
    Ok(())
}

pub(super) fn set_membership(
    txn: &WriteTransaction,
    group: &str,
    user: &str,
    member: bool,
) -> Result<()> {
    let mut groups = txn.open_table(GROUPS)?;
    let key = name_key(group);
    let mut record: Group = match groups.get(key.as_str())? {
        Some(v) => dec("groups", v.value())?,
        None => return Err(Error::NotFound(format!("group {group}"))),
    };
    record.members.retain(|m| !m.eq_ignore_ascii_case(user));
    if member {
        let name = match txn.open_table(USERS)?.get(name_key(user).as_str())? {
            Some(v) => dec::<User>("users", v.value())?.name,
            None => return Err(Error::NotFound(format!("user {user}"))),
        };
        record.members.push(name);
        record.members.sort();
    }
    groups.insert(key.as_str(), enc(&record).as_slice())?;
    Ok(())
}

pub(super) fn merge(txn: &WriteTransaction, from: &str, into: &str) -> Result<()> {
    let source = load_user(txn, from)?;
    no_edge_account(&source)?;
    let (from_key, into_key) = (name_key(&source.name), name_key(into));
    if from_key == into_key {
        return Err(Error::Invalid(format!(
            "{from} and {into} are the same user"
        )));
    }
    let existing = match txn.open_table(USERS)?.get(into_key.as_str())? {
        Some(v) => Some(dec::<User>("users", v.value())?),
        None => None,
    };
    let target = match existing {
        Some(target) => {
            no_edge_account(&target)?;
            if target.admin != source.admin {
                return Err(Error::Invalid(format!(
                    "only one of {} and {} is an administrator; make them alike first",
                    source.name, target.name
                )));
            }
            target
        }
        None => {
            super::apply::validate_name("user", into)?;
            match alias_of(txn, &into_key)? {
                Some(owner) if !owner.eq_ignore_ascii_case(&source.name) => {
                    return Err(Error::AlreadyExists(format!(
                        "user {into}, a former name of {owner}"
                    )));
                }
                _ => {}
            }
            User {
                name: into.to_owned(),
                ..source.clone()
            }
        }
    };
    let (old, new) = (source.name.as_str(), target.name.as_str());

    let mut links = txn.open_table(SSO_LINKS)?;
    let source_link = links.remove(from_key.as_str())?.map(|v| v.value().to_vec());
    if let Some(link) = source_link {
        if links.get(into_key.as_str())?.is_some() {
            return Err(Error::Invalid(format!(
                "both {old} and {new} are linked to single sign-on; unlink one first"
            )));
        }
        let identity: SsoIdentity = dec("sso_links", &link)?;
        links.insert(into_key.as_str(), link.as_slice())?;
        txn.open_table(SSO_SUBJECTS)?
            .insert((identity.issuer.as_str(), identity.subject.as_str()), new)?;
    }
    drop(links);
    rename_values(&mut txn.open_table(SSO_EXPECTED)?, old, new)?;

    // A disabled user stays disabled under the new name.
    let was_disabled = txn
        .open_table(DISABLED)?
        .remove(from_key.as_str())?
        .is_some();
    if was_disabled {
        txn.open_table(DISABLED)?.insert(into_key.as_str(), ())?;
    }
    let mut users = txn.open_table(USERS)?;
    users.remove(from_key.as_str())?;
    users.insert(into_key.as_str(), enc(&target).as_slice())?;
    drop(users);

    // The profile of `from` joins that of `into`, which wins where both
    // have a display name and keeps its primary address.
    let mut profiles = txn.open_table(PROFILES)?;
    let moved = profiles
        .remove(from_key.as_str())?
        .map(|v| v.value().to_vec());
    if let Some(moved) = moved {
        let moved: Profile = dec("profiles", &moved)?;
        let existing = profiles
            .get(into_key.as_str())?
            .map(|v| dec::<Profile>("profiles", v.value()))
            .transpose()?;
        let mut merged = existing.unwrap_or_default();
        merged.display_name = merged.display_name.or(moved.display_name);
        for email in moved.emails {
            if !merged.emails.iter().any(|e| e.eq_ignore_ascii_case(&email)) {
                merged.emails.push(email);
            }
        }
        profiles.insert(into_key.as_str(), enc(&merged).as_slice())?;
    }
    drop(profiles);
    rename_values(&mut txn.open_table(EMAIL_OWNERS)?, old, new)?;
    let mut aliases = txn.open_table(ALIASES)?;
    aliases.remove(into_key.as_str())?;
    rename_values(&mut aliases, old, new)?;
    aliases.insert(from_key.as_str(), new)?;
    drop(aliases);

    rewrite(txn, GROUPS, "groups", |g: &mut Group| {
        let found = rename(&mut g.members, old, new);
        if found {
            g.members.sort();
            g.members.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        }
        found
    })?;
    rewrite(txn, TOKENS, "tokens", |t: &mut Token| {
        rename_one(&mut t.user, old, new)
    })?;
    rewrite(txn, LOCKS, "locks", |l: &mut Lock| {
        rename_one(&mut l.holder.user, old, new)
    })?;
    rewrite(txn, SHELVES, "shelves", |s: &mut Shelf| {
        rename_one(&mut s.contents.owner, old, new)
    })?;
    rewrite(txn, REVIEWS, "reviews", |r: &mut Review| {
        let mut found = rename_one(&mut r.author, old, new);
        found |= rename(&mut r.reviewers, old, new);
        for vote in &mut r.votes {
            found |= rename_one(&mut vote.user, old, new);
        }
        if found {
            dedup_names(&mut r.reviewers);
            // A user has one vote; the latest of the merged ones counts.
            let mut seen = std::collections::BTreeSet::new();
            r.votes.reverse();
            r.votes.retain(|v| seen.insert(name_key(&v.user)));
            r.votes.reverse();
        }
        found
    })?;
    let mut settings = txn.open_table(SETTINGS)?;
    for key in [PROTECTIONS, REVIEW_RULES] {
        let source = settings
            .get(key)?
            .map(|v| String::from_utf8_lossy(v.value()).into_owned());
        if let Some(source) = source {
            let renamed = rename_subject(&source, old, new);
            if renamed != source {
                settings.insert(key, renamed.as_bytes())?;
            }
        }
    }
    Ok(())
}

fn rename_one(name: &mut String, old: &str, new: &str) -> bool {
    let found = name.eq_ignore_ascii_case(old);
    if found {
        *name = new.to_owned();
    }
    found
}

fn rename(names: &mut [String], old: &str, new: &str) -> bool {
    let mut found = false;
    for name in names {
        found |= rename_one(name, old, new);
    }
    found
}

fn dedup_names(names: &mut Vec<String>) {
    let mut seen = std::collections::BTreeSet::new();
    names.retain(|n| seen.insert(name_key(n)));
}

fn rename_values(table: &mut redb::Table<&str, &str>, old: &str, new: &str) -> Result<()> {
    let keys: Vec<String> = table
        .iter()?
        .filter_map(|r| r.ok())
        .filter(|(_, v)| v.value().eq_ignore_ascii_case(old))
        .map(|(k, _)| k.value().to_owned())
        .collect();
    for key in keys {
        table.insert(key.as_str(), new)?;
    }
    Ok(())
}

/// Applies `change` to every record of `table`, storing those it reports
/// as changed.
fn rewrite<K, T>(
    txn: &WriteTransaction,
    table: redb::TableDefinition<K, &[u8]>,
    name: &'static str,
    mut change: impl FnMut(&mut T) -> bool,
) -> Result<()>
where
    K: redb::Key + 'static,
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let mut table = txn.open_table(table)?;
    let mut changed = Vec::new();
    for row in table.iter()? {
        let (k, v) = row?;
        let mut record: T = dec(name, v.value())?;
        if change(&mut record) {
            changed.push((K::as_bytes(&k.value()).as_ref().to_vec(), enc(&record)));
        }
    }
    for (k, v) in changed {
        table.insert(K::from_bytes(&k), v.as_slice())?;
    }
    Ok(())
}

/// Replaces `user:<old>` with `user:<new>` in a rules table, keeping its
/// layout.
fn rename_subject(source: &str, old: &str, new: &str) -> String {
    let wanted = format!("user:{old}");
    source
        .split_inclusive(char::is_whitespace)
        .map(|piece| {
            let word = piece.trim_end();
            match word.eq_ignore_ascii_case(&wanted) {
                true => format!("user:{new}{}", &piece[word.len()..]),
                false => piece.to_owned(),
            }
        })
        .collect()
}

/// Expects the users that single sign-on created before account links
/// existed to link on their next sign-on under the same name.
pub(super) fn expect_sso_created_users(txn: &WriteTransaction) -> Result<()> {
    let mut created = Vec::new();
    {
        let audit = txn.open_table(AUDIT)?;
        let log = txn.open_table(LOG)?;
        for row in audit.iter()? {
            let (index, entry) = row?;
            let entry: AuditEntry = dec("audit", entry.value())?;
            if entry.actor != "oidc" {
                continue;
            }
            let Some(command) = log.get(index.value())? else {
                continue;
            };
            if let Command::CreateUser {
                name,
                password_hash: None,
                ..
            } = dec("log", command.value())?
            {
                created.push(name);
            }
        }
    }
    let users = txn.open_table(USERS)?;
    let mut expected = txn.open_table(SSO_EXPECTED)?;
    for name in created {
        let key = name_key(&name);
        if let Some(user) = users.get(key.as_str())? {
            let user: User = dec("users", user.value())?;
            expected.insert(key.as_str(), user.name.as_str())?;
        }
    }
    Ok(())
}

impl Reader {
    /// Returns the user linked to a single sign-on identity.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn sso_user(&self, issuer: &str, subject: &str) -> Result<Option<String>> {
        Ok(self
            .txn()
            .open_table(SSO_SUBJECTS)?
            .get((issuer, subject))?
            .map(|v| v.value().to_owned()))
    }

    /// Returns the single sign-on identity `user` is linked to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn sso_link(&self, user: &str) -> Result<Option<SsoIdentity>> {
        match self
            .txn()
            .open_table(SSO_LINKS)?
            .get(name_key(user).as_str())?
        {
            Some(v) => Ok(Some(dec("sso_links", v.value())?)),
            None => Ok(None),
        }
    }

    /// Returns the user that a first single sign-on as `sso_name` is
    /// expected to link to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn expected_sso(&self, sso_name: &str) -> Result<Option<String>> {
        Ok(self
            .txn()
            .open_table(SSO_EXPECTED)?
            .get(name_key(sso_name).as_str())?
            .map(|v| v.value().to_owned()))
    }

    /// Returns the single sign-on name a first sign-on is expected under to
    /// link to `user`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn expected_sso_of(&self, user: &str) -> Result<Option<String>> {
        for row in self.txn().open_table(SSO_EXPECTED)?.iter()? {
            let (k, v) = row?;
            if v.value().eq_ignore_ascii_case(user) {
                return Ok(Some(k.value().to_owned()));
            }
        }
        Ok(None)
    }

    /// Returns whether `user` is disabled.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn is_disabled(&self, user: &str) -> Result<bool> {
        Ok(self
            .txn()
            .open_table(DISABLED)?
            .get(name_key(user).as_str())?
            .is_some())
    }

    /// Returns all users, ordered by folded name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn users(&self) -> Result<Vec<User>> {
        self.txn()
            .open_table(USERS)?
            .iter()?
            .map(|r| dec("users", r?.1.value()))
            .collect()
    }

    /// Returns the profile of `user`, empty if they have set nothing. A
    /// former name finds the profile of the user it became, since history
    /// keeps the name a change was recorded under.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn profile_of(&self, user: &str) -> Result<Profile> {
        let mut key = name_key(user);
        if let Some(owner) = self.txn().open_table(ALIASES)?.get(key.as_str())? {
            key = name_key(owner.value());
        }
        match self.txn().open_table(PROFILES)?.get(key.as_str())? {
            Some(v) => dec("profiles", v.value()),
            None => Ok(Profile::default()),
        }
    }

    /// Returns the user that has the address `email`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn email_owner(&self, email: &str) -> Result<Option<String>> {
        Ok(self
            .txn()
            .open_table(EMAIL_OWNERS)?
            .get(email.trim().to_lowercase().as_str())?
            .map(|v| v.value().to_owned()))
    }

    /// Returns the user that `name` is a former name of.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn alias(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .txn()
            .open_table(ALIASES)?
            .get(name_key(name).as_str())?
            .map(|v| v.value().to_owned()))
    }

    /// Returns the former names of `user`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn aliases_of(&self, user: &str) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for row in self.txn().open_table(ALIASES)?.iter()? {
            let (k, v) = row?;
            if v.value().eq_ignore_ascii_case(user) {
                names.push(k.value().to_owned());
            }
        }
        Ok(names)
    }
}
