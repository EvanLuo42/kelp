//! Pre-submit review rules.
//!
//! A review rules table says which changes need a review before they can be
//! submitted, and what the review needs. Each line is
//! `approve <count> <who> <path>`, `status <context> <path> [by <who>]` or
//! `issue <path>`:
//!
//! ```text
//! approve 1  group:engine-leads  //engine/...
//! approve 2  *                   //game/main/Content/...
//! status  build                  //game/...   by group:ci
//! issue                          //game/release/...
//! ```
//!
//! A change needs every requirement whose path matches one of its files. An
//! `approve` requirement is met by approvals of the review's latest version
//! from `count` users that `who` names, not counting the author. A `status`
//! requirement is met when the latest report of `context` for the latest
//! version, typically from CI, is a success. With `by`, only reports from
//! users that `who` names count, so that an author cannot report success
//! for their own change; without it, anyone who may write the files can
//! report. A rejection of the latest
//! version blocks the submit. An `issue` requirement needs the changelist to
//! be linked to an issue, and needs no review. Administrators are exempt.

use std::collections::BTreeSet;

use kelp_core::DepotPath;

use crate::acl::{PathPattern, Subject, Who};
use crate::record::{CheckState, Review};
use crate::{Error, Result};

/// A parsed review rules table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewRules {
    rules: Vec<(Requirement, PathPattern)>,
}

/// Something a review must have before its change can be submitted.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Requirement {
    /// Approvals from `count` users named by `from`.
    Approvals { count: u32, from: Who },
    /// A successful status report of `context`, from a user named by `by`
    /// if given.
    Status { context: String, by: Option<Who> },
    /// A link to an issue.
    Issue,
}

impl Requirement {
    /// Returns `true` if a review meets the requirement, rather than the
    /// changelist itself.
    pub fn needs_review(&self) -> bool {
        !matches!(self, Self::Issue)
    }
}

impl Ord for Who {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.to_string().cmp(&other.to_string())
    }
}

impl PartialOrd for Who {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Approvals { count, from } => write!(f, "{count} approval(s) from {from}"),
            Self::Status { context, by: None } => write!(f, "status {context} succeeded"),
            Self::Status {
                context,
                by: Some(by),
            } => write!(f, "status {context} succeeded, reported by {by}"),
            Self::Issue => f.write_str("a linked issue"),
        }
    }
}

/// Whether a review meets one [`Requirement`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub requirement: String,
    pub met: bool,
}

impl ReviewRules {
    /// Parses a review rules table.
    ///
    /// Blank lines and lines starting with `#` are ignored.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if a line is malformed.
    ///
    /// # Examples
    ///
    /// ```
    /// use kelp_core::DepotPath;
    /// use kelp_meta::review::ReviewRules;
    ///
    /// let rules = ReviewRules::parse("approve 1 group:leads //engine/...\nstatus build //...").unwrap();
    /// let path = DepotPath::parse("//engine/main/render.rs").unwrap();
    /// assert_eq!(rules.requirements([&path]).len(), 2);
    /// ```
    pub fn parse(src: &str) -> Result<Self> {
        let mut rules = Vec::new();
        for line in src
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let bad = |why: &str| Error::Invalid(format!("review rules line {line:?}: {why}"));
            let parts: Vec<&str> = line.split_whitespace().collect();
            let (requirement, path) = match parts[..] {
                ["approve", count, who, path] => {
                    let count = count
                        .parse()
                        .ok()
                        .filter(|&c| c > 0)
                        .ok_or_else(|| bad("the count must be a positive number"))?;
                    let from = Who::parse(who)
                        .ok_or_else(|| bad("who must be *, user:<name> or group:<name>"))?;
                    (Requirement::Approvals { count, from }, path)
                }
                ["status", context, path] => (
                    Requirement::Status {
                        context: context.to_owned(),
                        by: None,
                    },
                    path,
                ),
                ["status", context, path, "by", who] => {
                    let by = Who::parse(who)
                        .ok_or_else(|| bad("who must be *, user:<name> or group:<name>"))?;
                    let context = context.to_owned();
                    (
                        Requirement::Status {
                            context,
                            by: Some(by),
                        },
                        path,
                    )
                }
                ["issue", path] => (Requirement::Issue, path),
                _ => {
                    return Err(bad(
                        "expected `approve <count> <who> <path>`, `status <context> <path> [by <who>]` or `issue <path>`",
                    ));
                }
            };
            rules.push((requirement, PathPattern::parse(path).map_err(|e| bad(&e))?));
        }
        Ok(Self { rules })
    }

    /// Returns `true` if the table has no rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Returns the requirements of a change to `paths`, without duplicates.
    pub fn requirements<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a DepotPath>,
    ) -> Vec<Requirement> {
        let paths: Vec<&DepotPath> = paths.into_iter().collect();
        let found: BTreeSet<&Requirement> = self
            .rules
            .iter()
            .filter(|(_, pattern)| paths.iter().any(|p| pattern.matches(p)))
            .map(|(r, _)| r)
            .collect();
        found.into_iter().cloned().collect()
    }
}

/// Checks `review` against `requirements`, looking up users' groups with
/// `groups_of`. A rejection of the latest version is reported as an unmet
/// check of its own.
pub fn evaluate(
    review: &Review,
    requirements: &[Requirement],
    groups_of: &dyn Fn(&str) -> Vec<String>,
) -> Vec<Check> {
    let version = review.version();
    let current = || review.votes.iter().filter(move |v| v.version == version);
    let rejecters: BTreeSet<String> = current()
        .filter(|v| !v.approve)
        .map(|v| v.user.to_lowercase())
        .collect();
    let mut checks: Vec<Check> = rejecters
        .into_iter()
        .map(|user| Check {
            requirement: format!("not rejected by {user}"),
            met: false,
        })
        .collect();
    for requirement in requirements.iter().filter(|r| r.needs_review()) {
        let met = match requirement {
            Requirement::Issue => unreachable!("filtered out"),
            Requirement::Approvals { count, from } => {
                let approvers = current()
                    .filter(|v| v.approve && !v.user.eq_ignore_ascii_case(&review.author))
                    .filter(|v| {
                        let groups = groups_of(&v.user);
                        from.includes(Subject {
                            user: &v.user,
                            groups: &groups,
                        })
                    })
                    .map(|v| v.user.to_lowercase())
                    .collect::<BTreeSet<_>>()
                    .len();
                approvers >= *count as usize
            }
            Requirement::Status { context, by } => {
                let version = review.version();
                let reported = review.statuses.iter().rev().find(|s| {
                    s.version == version
                        && s.context == *context
                        && by.as_ref().is_none_or(|by| {
                            let groups = groups_of(&s.by);
                            by.includes(Subject {
                                user: &s.by,
                                groups: &groups,
                            })
                        })
                });
                reported.map(|s| s.state) == Some(CheckState::Success)
            }
        };
        checks.push(Check {
            requirement: requirement.to_string(),
            met,
        });
    }
    checks
}
