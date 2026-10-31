use kelp_core::{ClNumber, DepotPath};
use redb::{ReadableTable, WriteTransaction};

use crate::db::{
    GROUPS, REVIEW_RULES, REVIEWS, Reader, SETTINGS, SHELVES, USERS, dec, enc, name_key,
};
use crate::record::{Group, Import, Review, ReviewState, ReviewVersion, Shelf, User, Vote};
use crate::review::{Check, Requirement, ReviewRules, evaluate};
use crate::{Error, FileChange, Governance, Result, Submission};

pub(in crate::db) fn apply(txn: &WriteTransaction, cmd: &Governance) -> Result<()> {
    match cmd {
        Governance::OpenReview {
            shelf,
            reviewers,
            by,
            at,
        } => {
            let record: Shelf = match txn.open_table(SHELVES)?.get(shelf.0)? {
                Some(v) => dec("shelves", v.value())?,
                None => return Err(Error::NotFound(format!("shelf {shelf}"))),
            };
            if name_key(&record.contents.owner) != name_key(by) {
                return Err(Error::Invalid(format!(
                    "shelf {shelf} belongs to {}",
                    record.contents.owner
                )));
            }
            if let Some(existing) = load(txn, *shelf)?
                && existing.state == ReviewState::Open
            {
                return Err(Error::AlreadyExists(format!("review {shelf}")));
            }
            let users = txn.open_table(USERS)?;
            for reviewer in reviewers {
                if users.get(name_key(reviewer).as_str())?.is_none() {
                    return Err(Error::NotFound(format!("user {reviewer}")));
                }
            }
            let c = record.contents;
            save(
                txn,
                &Review {
                    shelf: *shelf,
                    stream: c.stream,
                    author: c.owner,
                    description: c.description.clone(),
                    reviewers: reviewers.clone(),
                    versions: vec![ReviewVersion {
                        files: c.files,
                        description: c.description,
                        at: *at,
                    }],
                    votes: Vec::new(),
                    statuses: Vec::new(),
                    state: ReviewState::Open,
                    opened_at: *at,
                },
            )?;
        }
        Governance::Vote {
            shelf,
            user,
            approve,
            at,
        }
        | Governance::VoteOn {
            shelf,
            user,
            approve,
            at,
            ..
        } => {
            let mut review = open(txn, *shelf)?;
            if let Governance::VoteOn { version, .. } = cmd
                && *version != review.version()
            {
                return Err(Error::Conflict(vec![format!(
                    "review {shelf} has a newer version {}; look at it again",
                    review.version()
                )]));
            }
            review.votes.retain(|v| name_key(&v.user) != name_key(user));
            if let Some(approve) = approve {
                review.votes.push(Vote {
                    user: user.clone(),
                    version: review.version(),
                    approve: *approve,
                    at: *at,
                });
            }
            save(txn, &review)?;
        }
        Governance::ReportStatus {
            shelf,
            version,
            context,
            state,
            url,
            by,
            at,
        } => {
            let mut review = open(txn, *shelf)?;
            if *version == 0 || *version > review.version() {
                return Err(Error::Invalid(format!(
                    "review {shelf} has no version {version}"
                )));
            }
            review.statuses.push(crate::record::StatusReport {
                context: context.clone(),
                version: *version,
                state: *state,
                url: url.clone(),
                by: by.clone(),
                at: *at,
            });
            save(txn, &review)?;
        }
        Governance::CloseReview { shelf } => {
            let mut review = open(txn, *shelf)?;
            review.state = ReviewState::Closed;
            save(txn, &review)?;
        }
        Governance::SetReviewRules { source } => {
            ReviewRules::parse(source)?;
            txn.open_table(SETTINGS)?
                .insert(REVIEW_RULES, source.as_bytes())?;
        }
        _ => unreachable!("not a review command"),
    }
    Ok(())
}

/// Adds a version to the open review of `shelf`, if there is one, after the
/// shelf was shelved again.
pub(in crate::db) fn shelved(txn: &WriteTransaction, shelf: &Shelf) -> Result<()> {
    if let Some(mut review) = load(txn, shelf.number)?
        && review.state == ReviewState::Open
    {
        review.versions.push(ReviewVersion {
            files: shelf.contents.files.clone(),
            description: shelf.contents.description.clone(),
            at: shelf.contents.at,
        });
        review.description = shelf.contents.description.clone();
        save(txn, &review)?;
    }
    Ok(())
}

/// Closes the open review of `shelf`, if there is one, after the shelf was
/// deleted.
pub(in crate::db) fn shelf_deleted(txn: &WriteTransaction, shelf: ClNumber) -> Result<()> {
    if let Some(mut review) = load(txn, shelf)?
        && review.state == ReviewState::Open
    {
        review.state = ReviewState::Closed;
        save(txn, &review)?;
    }
    Ok(())
}

/// Checks that `submission` meets the review rules, before changelist `cl`
/// is recorded, and completes its review.
///
/// # Errors
///
/// Returns [`Error::Conflict`] listing the unmet requirements, or
/// [`Error::Invalid`] if the named review does not match the submit.
pub(in crate::db) fn gate(
    txn: &WriteTransaction,
    cl: ClNumber,
    submission: &Submission,
) -> Result<()> {
    let rules = match txn.open_table(SETTINGS)?.get(REVIEW_RULES)? {
        Some(v) => ReviewRules::parse(&String::from_utf8_lossy(v.value()))?,
        None => ReviewRules::default(),
    };
    let paths = governed_paths(
        &super::super::imports::load(txn, &submission.stream)?,
        &submission.stream,
        &submission.changes,
    )?;
    let mut requirements = rules.requirements(&paths);
    let exempt = match txn
        .open_table(USERS)?
        .get(name_key(&submission.author).as_str())?
    {
        Some(v) => dec::<User>("users", v.value())?.admin,
        None => false,
    };
    if requirements.contains(&Requirement::Issue) && submission.issues.is_empty() && !exempt {
        return Err(Error::Conflict(vec![
            "this change needs a linked issue; mention one in the description or pass --fixes"
                .into(),
        ]));
    }
    requirements.retain(Requirement::needs_review);
    let review = match submission.review {
        Some(shelf) => load(txn, shelf)?.filter(|r| r.state == ReviewState::Open),
        None => None,
    };
    let Some(mut review) = review else {
        if requirements.is_empty() || exempt {
            return Ok(());
        }
        let needs: Vec<String> = requirements.iter().map(ToString::to_string).collect();
        return Err(Error::Conflict(vec![format!(
            "this change needs a review with {}",
            needs.join(", ")
        )]));
    };
    let shelf = review.shelf;
    if review.stream != submission.stream {
        return Err(Error::Invalid(format!(
            "review {shelf} is for {}",
            review.stream
        )));
    }
    if !exempt && name_key(&review.author) != name_key(&submission.author) {
        return Err(Error::Invalid(format!(
            "review {shelf} belongs to {}",
            review.author
        )));
    }
    if sorted(review.files()) != sorted(&submission.changes) {
        return Err(Error::Conflict(vec![format!(
            "the files differ from version {} of review {shelf}; shelve again",
            review.version()
        )]));
    }
    if !exempt {
        let groups = groups(txn)?;
        let unmet: Vec<String> = evaluate(&review, &requirements, &|u| groups_of(&groups, u))
            .into_iter()
            .filter(|c| !c.met)
            .map(|c| c.requirement)
            .collect();
        if !unmet.is_empty() {
            return Err(Error::Conflict(
                unmet
                    .into_iter()
                    .map(|r| format!("review {shelf} needs {r}"))
                    .collect(),
            ));
        }
    }
    review.state = ReviewState::Submitted(cl);
    save(txn, &review)?;
    txn.open_table(SHELVES)?.remove(shelf.0)?;
    Ok(())
}

/// Returns the paths review rules apply to for `changes` in `stream`: each
/// path, and for a path below an import, the path it stands for in the
/// import's source, so the source's rules cannot be sidestepped through a
/// writable import.
fn governed_paths(
    imports: &[Import],
    stream: &DepotPath,
    changes: &[FileChange],
) -> Result<Vec<DepotPath>> {
    let mut paths = Vec::with_capacity(changes.len());
    for change in changes {
        paths.push(change.path.clone());
        let Some(rel) = super::super::stream_relative(stream, &change.path) else {
            continue;
        };
        if let Some((import, source_rel)) = super::super::imports::route(imports, &rel) {
            paths.push(match source_rel.is_empty() {
                true => import.source.clone(),
                false => import.source.join(&source_rel)?,
            });
        }
    }
    Ok(paths)
}

fn sorted(files: &[FileChange]) -> Vec<&FileChange> {
    let mut files: Vec<&FileChange> = files.iter().collect();
    files.sort_by(|a, b| a.path.key().cmp(b.path.key()));
    files
}

fn groups(txn: &WriteTransaction) -> Result<Vec<Group>> {
    txn.open_table(GROUPS)?
        .iter()?
        .map(|r| dec("groups", r?.1.value()))
        .collect()
}

fn groups_of(groups: &[Group], user: &str) -> Vec<String> {
    groups
        .iter()
        .filter(|g| g.members.iter().any(|m| name_key(m) == name_key(user)))
        .map(|g| g.name.clone())
        .collect()
}

fn load(txn: &WriteTransaction, shelf: ClNumber) -> Result<Option<Review>> {
    txn.open_table(REVIEWS)?
        .get(shelf.0)?
        .map(|v| dec("reviews", v.value()))
        .transpose()
}

fn open(txn: &WriteTransaction, shelf: ClNumber) -> Result<Review> {
    match load(txn, shelf)? {
        Some(r) if r.state == ReviewState::Open => Ok(r),
        Some(_) => Err(Error::Invalid(format!("review {shelf} is closed"))),
        None => Err(Error::NotFound(format!("review {shelf}"))),
    }
}

fn save(txn: &WriteTransaction, review: &Review) -> Result<()> {
    txn.open_table(REVIEWS)?
        .insert(review.shelf.0, enc(review).as_slice())?;
    Ok(())
}

impl Reader {
    /// Returns the review of shelf `shelf`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn review(&self, shelf: ClNumber) -> Result<Option<Review>> {
        self.txn()
            .open_table(REVIEWS)?
            .get(shelf.0)?
            .map(|v| dec("reviews", v.value()))
            .transpose()
    }

    /// Returns the reviews of `stream`, or of all streams, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn reviews(&self, stream: Option<&DepotPath>, open_only: bool) -> Result<Vec<Review>> {
        let mut out = Vec::new();
        for row in self.txn().open_table(REVIEWS)?.iter()?.rev() {
            let review: Review = dec("reviews", row?.1.value())?;
            if stream.is_none_or(|s| *s == review.stream)
                && (!open_only || review.state == ReviewState::Open)
            {
                out.push(review);
            }
        }
        Ok(out)
    }

    /// Returns the source of the review rules table, if one was set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn review_rules_source(&self) -> Result<Option<String>> {
        Ok(self
            .txn()
            .open_table(SETTINGS)?
            .get(REVIEW_RULES)?
            .map(|v| String::from_utf8_lossy(v.value()).into_owned()))
    }

    /// Checks `review` against the current review rules.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn review_checks(&self, review: &Review) -> Result<Vec<Check>> {
        let rules = match self.review_rules_source()? {
            Some(source) => ReviewRules::parse(&source)?,
            None => ReviewRules::default(),
        };
        let imports = self.imports(&review.stream)?;
        let paths = governed_paths(&imports, &review.stream, review.files())?;
        let mut requirements = rules.requirements(&paths);
        requirements.retain(Requirement::needs_review);
        let groups = self.groups()?;
        Ok(evaluate(review, &requirements, &|u| groups_of(&groups, u)))
    }
}
