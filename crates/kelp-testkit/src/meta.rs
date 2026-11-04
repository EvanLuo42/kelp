//! A metadata database in a sandbox, with helpers for building changes.
//!
//! Enabled by the `meta` feature.

use kelp_core::{ClNumber, ContentId, FileManifest};
use kelp_meta::{
    Command, FileAction, FileChange, FileData, Integration, MetaDb, Result, StreamKind, Submission,
};

use crate::{Sandbox, depot_path};

/// A [`MetaDb`] with depot `game` and mainline `//game/main`.
pub struct MetaFixture {
    db: MetaDb,
    _sandbox: Sandbox,
}

impl MetaFixture {
    /// Creates the database, the depot and the mainline.
    ///
    /// # Panics
    ///
    /// Panics if the database cannot be created.
    pub fn new() -> Self {
        let sandbox = Sandbox::new();
        let db = MetaDb::open(&sandbox.path("meta.redb")).expect("open database");
        db.commit(Command::CreateDepot {
            name: "game".into(),
            at: 1,
        })
        .expect("create depot");
        let main = Command::CreateStream {
            path: depot_path("//game/main"),
            kind: StreamKind::Mainline,
            parent: None,
            at: 1,
        };
        db.commit(main).expect("create mainline");
        Self {
            db,
            _sandbox: sandbox,
        }
    }

    /// Returns the database.
    pub fn db(&self) -> &MetaDb {
        &self.db
    }

    /// Closes the database and returns the sandbox that holds its file,
    /// `meta.redb`.
    pub fn close(self) -> Sandbox {
        drop(self.db);
        self._sandbox
    }

    /// Creates a stream of `kind` under `parent`.
    ///
    /// # Panics
    ///
    /// Panics if the stream cannot be created.
    pub fn create_stream(&self, path: &str, kind: StreamKind, parent: &str) {
        let cmd = Command::CreateStream {
            path: depot_path(path),
            kind,
            parent: Some(depot_path(parent)),
            at: 1,
        };
        self.db.commit(cmd).expect("create stream");
    }

    /// Stores a manifest for `bytes` and returns its file data.
    ///
    /// # Panics
    ///
    /// Panics if the manifest cannot be stored.
    pub fn data(&self, bytes: &[u8]) -> FileData {
        let content = ContentId::of(bytes);
        let size = bytes.len() as u64;
        let manifest = FileManifest {
            content,
            size,
            chunks: Vec::new(),
        };
        self.db.put_manifest(&manifest).expect("store manifest");
        FileData {
            content,
            size,
            executable: false,
        }
    }

    /// Returns a change adding `path` with `bytes`.
    pub fn add(&self, path: &str, bytes: &[u8]) -> FileChange {
        FileChange {
            path: depot_path(path),
            action: FileAction::Add {
                data: self.data(bytes),
            },
        }
    }

    /// Returns a change replacing `base` with `bytes` at `path`.
    pub fn edit(&self, path: &str, base: &[u8], bytes: &[u8]) -> FileChange {
        let action = FileAction::Edit {
            base: ContentId::of(base),
            data: self.data(bytes),
        };
        FileChange {
            path: depot_path(path),
            action,
        }
    }

    /// Returns a change deleting `path`, whose content is `base`.
    pub fn delete(&self, path: &str, base: &[u8]) -> FileChange {
        FileChange {
            path: depot_path(path),
            action: FileAction::Delete {
                base: ContentId::of(base),
            },
        }
    }

    /// Submits `changes` to `stream` as `user` from workspace `ws`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`MetaDb::submit`].
    pub fn submit_as(
        &self,
        stream: &str,
        user: &str,
        ws: &str,
        changes: Vec<FileChange>,
        integration: Option<(&str, u64)>,
    ) -> Result<ClNumber> {
        let submission = Submission {
            stream: depot_path(stream),
            author: user.to_owned(),
            workspace: ws.to_owned(),
            description: "test".into(),
            changes,
            integration: integration.map(|(source, cl)| Integration {
                source: depot_path(source),
                through: ClNumber(cl),
            }),
            issues: Vec::new(),
            fixes: Vec::new(),
            review: None,
            at: 2,
        };
        self.db.submit(submission).map(|(_, cl, _)| cl)
    }

    /// Submits `changes` to `stream` as `alice`, linked to `issues` and
    /// fixing `fixes`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`MetaDb::submit`].
    pub fn submit_linked(
        &self,
        stream: &str,
        changes: Vec<FileChange>,
        issues: &[&str],
        fixes: &[&str],
    ) -> Result<ClNumber> {
        let owned = |keys: &[&str]| keys.iter().map(|k| k.to_string()).collect();
        let submission = Submission {
            stream: depot_path(stream),
            author: "alice".into(),
            workspace: "ws".into(),
            description: "test".into(),
            changes,
            integration: None,
            issues: owned(issues),
            fixes: owned(fixes),
            review: None,
            at: 2,
        };
        self.db.submit(submission).map(|(_, cl, _)| cl)
    }

    /// Submits `changes` to `stream` as `user` from workspace `ws`, as the
    /// latest version of review `review`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`MetaDb::submit`].
    pub fn submit_reviewed(
        &self,
        stream: &str,
        user: &str,
        changes: Vec<FileChange>,
        review: Option<u64>,
    ) -> Result<ClNumber> {
        let submission = Submission {
            stream: depot_path(stream),
            author: user.to_owned(),
            workspace: "ws".into(),
            description: "test".into(),
            changes,
            integration: None,
            issues: Vec::new(),
            fixes: Vec::new(),
            review: review.map(ClNumber),
            at: 2,
        };
        self.db.submit(submission).map(|(_, cl, _)| cl)
    }

    /// Submits `changes` to `stream` as `alice` from workspace `ws`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`MetaDb::submit`].
    pub fn submit(&self, stream: &str, changes: Vec<FileChange>) -> Result<ClNumber> {
        self.submit_as(stream, "alice", "ws", changes, None)
    }
}

impl Default for MetaFixture {
    fn default() -> Self {
        Self::new()
    }
}
