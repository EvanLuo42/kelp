//! The C ABI for engine, editor and DCC integrations.
//!
//! See `include/kelp.h` for the C declarations and their documentation.
//! Operations go to kelpd when it is running, and otherwise run inside the
//! calling process. Every function may be called from any thread; calls on
//! one workspace handle are serialized.
//!
//! Functions return [`KELP_OK`] or [`KELP_ERROR`]; after an error,
//! [`kelp_last_error`] describes it until the next call on the same thread.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use kelp_client::ops::{Op, Outcome, Session};
use kelp_client::{FileReport, Home, ipc};
use kelp_core::LocalClId;
use kelp_workspace::OpenAction;
use tokio::runtime::Runtime;

/// The version of this C ABI. It changes when the ABI breaks.
pub const KELP_ABI_VERSION: u32 = 1;

/// The call succeeded.
pub const KELP_OK: i32 = 0;
/// The call failed; see [`kelp_last_error`].
pub const KELP_ERROR: i32 = 1;

/// The state of a file as seen by [`kelp_file_info`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KelpFileState {
    /// The file is neither in the workspace nor on disk.
    Unknown = 0,
    /// The file is on disk but not in the workspace.
    Untracked = 1,
    /// The file matches what the workspace has.
    Synced = 2,
    /// The file was changed on disk without being opened.
    Modified = 3,
    /// The file is missing from disk without being opened for delete.
    Missing = 4,
    OpenedForAdd = 5,
    OpenedForEdit = 6,
    OpenedForDelete = 7,
}

/// Information about one file, filled in by [`kelp_file_info`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KelpFileInfo {
    pub state: KelpFileState,
    /// The pending changelist the file is opened in; 0 is the default changelist.
    pub changelist: u32,
    /// The file needs a resolve before it can be submitted.
    pub needs_resolve: bool,
    /// The typemap requires an exclusive lock to edit the file.
    pub exclusive: bool,
    /// Someone holds the file's lock.
    pub locked: bool,
    /// This workspace holds the file's lock.
    pub locked_here: bool,
    /// The stream has a newer revision of the file.
    pub out_of_date: bool,
    /// Who holds the lock, as `user@workspace`, NUL-terminated and truncated
    /// to fit; empty if unlocked.
    pub locked_by: [c_char; 128],
}

/// A handle to a workspace, created by [`kelp_workspace_open`].
pub struct KelpWorkspace {
    root: PathBuf,
    home: Home,
    session: Session,
    lock: Mutex<()>,
}

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kelp-sdk")
            .enable_all()
            .build()
            .expect("start the Kelp SDK runtime")
    })
}

/// Runs `f`, turning a panic into [`KELP_ERROR`] so that it never unwinds
/// into the caller's C code.
fn guard(f: impl FnOnce() -> i32) -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| fail("internal error in the Kelp SDK"))
}

fn fail(message: impl std::fmt::Display) -> i32 {
    let text = CString::new(message.to_string().replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with_borrow_mut(|e| *e = text);
    KELP_ERROR
}

/// Reads a NUL-terminated UTF-8 string.
///
/// # Safety
///
/// `s` must be null or point to a NUL-terminated string.
unsafe fn read_str<'a>(s: *const c_char) -> Result<&'a str, String> {
    if s.is_null() {
        return Err("null string argument".into());
    }
    // SAFETY: the caller guarantees `s` is a valid NUL-terminated string.
    unsafe { CStr::from_ptr(s) }
        .to_str()
        .map_err(|_| "string argument is not UTF-8".into())
}

/// Reads `count` path strings.
///
/// # Safety
///
/// `paths` must point to `count` valid string pointers.
unsafe fn read_paths(
    ws: &KelpWorkspace,
    paths: *const *const c_char,
    count: usize,
) -> Result<Vec<PathBuf>, String> {
    if paths.is_null() && count > 0 {
        return Err("null path array".into());
    }
    (0..count)
        .map(|i| {
            // SAFETY: the caller guarantees `paths` has `count` valid entries.
            let p = unsafe { read_str(*paths.add(i)) }?;
            Ok(ws.root.join(p))
        })
        .collect()
}

impl KelpWorkspace {
    fn run(&self, op: Op) -> Result<Outcome, String> {
        let _serial = self.lock.lock().map_err(|_| "workspace handle poisoned")?;
        let work = async {
            match ipc::call(&self.home, &self.root, op.clone()).await {
                Err(kelp_client::Error::Io(_) | kelp_client::Error::DaemonVersion(_)) => self
                    .session
                    .execute(&self.root, op)
                    .await
                    .map_err(|e| e.to_string()),
                other => other.map_err(|e| e.to_string()),
            }
        };
        // A host already running a Tokio runtime on this thread cannot block on ours.
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|s| s.spawn(|| runtime().block_on(work)).join())
                .map_err(|_| "SDK worker thread panicked".to_owned())?
        } else {
            runtime().block_on(work)
        }
    }
}

/// Returns [`KELP_ABI_VERSION`].
#[unsafe(no_mangle)]
pub extern "C" fn kelp_abi_version() -> u32 {
    KELP_ABI_VERSION
}

/// Returns the SDK version as a static NUL-terminated string.
#[unsafe(no_mangle)]
pub extern "C" fn kelp_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Returns the last error on this thread as a NUL-terminated string, valid
/// until the next SDK call on this thread.
#[unsafe(no_mangle)]
pub extern "C" fn kelp_last_error() -> *const c_char {
    LAST_ERROR.with_borrow(|e| e.as_ptr())
}

/// Opens the workspace containing `path`, using the Kelp home `home`, or the
/// default home if `home` is null. Returns null on error.
///
/// # Safety
///
/// `path` must be a NUL-terminated UTF-8 string; `home` must be null or one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kelp_workspace_open(
    path: *const c_char,
    home: *const c_char,
) -> *mut KelpWorkspace {
    // SAFETY: forwarded from the caller's guarantees.
    let opened = (|| -> Result<KelpWorkspace, String> {
        let path = unsafe { read_str(path) }?;
        let home = match home.is_null() {
            true => Home::default_location().ok_or("cannot find the Kelp home; set KELP_HOME")?,
            false => Home::new(unsafe { read_str(home) }?),
        };
        let root = kelp_client::Workspace::discover(Path::new(path))
            .map_err(|e| e.to_string())?
            .root()
            .to_owned();
        Ok(KelpWorkspace {
            root,
            session: Session::new(home.clone()),
            home,
            lock: Mutex::new(()),
        })
    })();
    match opened {
        Ok(ws) => Box::into_raw(Box::new(ws)),
        Err(e) => {
            fail(e);
            std::ptr::null_mut()
        }
    }
}

/// Closes a workspace handle. Null is ignored.
///
/// # Safety
///
/// `ws` must be null or a handle from [`kelp_workspace_open`] that has not
/// been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kelp_workspace_close(ws: *mut KelpWorkspace) {
    if !ws.is_null() {
        // SAFETY: the caller passes a live handle created by Box::into_raw.
        drop(unsafe { Box::from_raw(ws) });
    }
}

/// Fills `out` with the state of the file at `path`, relative to the
/// workspace root or absolute.
///
/// # Safety
///
/// `ws` must be a live handle, `path` a NUL-terminated UTF-8 string and
/// `out` a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kelp_file_info(
    ws: *const KelpWorkspace,
    path: *const c_char,
    out: *mut KelpFileInfo,
) -> i32 {
    guard(|| {
        // SAFETY: forwarded from the caller's guarantees.
        let (Some(ws), false) = (unsafe { ws.as_ref() }, out.is_null()) else {
            return fail("null argument");
        };
        let path = match unsafe { read_str(path) } {
            Ok(p) => ws.root.join(p),
            Err(e) => return fail(e),
        };
        match ws.run(Op::Inspect { paths: vec![path] }) {
            Ok(Outcome::Files(reports)) if reports.len() == 1 => {
                // SAFETY: `out` is non-null and valid per the caller.
                unsafe { *out = file_info(&reports[0]) };
                KELP_OK
            }
            Ok(_) => fail("unexpected reply"),
            Err(e) => fail(e),
        }
    })
}

fn file_info(r: &FileReport) -> KelpFileInfo {
    let state = match (r.opened, r.tracked, r.exists, r.modified) {
        (Some((OpenAction::Add, _)), ..) => KelpFileState::OpenedForAdd,
        (Some((OpenAction::Delete | OpenAction::MoveFrom, _)), ..) => {
            KelpFileState::OpenedForDelete
        }
        (Some(_), ..) => KelpFileState::OpenedForEdit,
        (None, true, false, _) => KelpFileState::Missing,
        (None, true, true, true) => KelpFileState::Modified,
        (None, true, true, false) => KelpFileState::Synced,
        (None, false, true, _) => KelpFileState::Untracked,
        (None, false, false, _) => KelpFileState::Unknown,
    };
    let mut locked_by = [0 as c_char; 128];
    if let Some((user, workspace)) = &r.locked_by {
        let text = format!("{user}@{workspace}");
        for (dst, src) in locked_by.iter_mut().zip(text.bytes().take(127)) {
            *dst = src as c_char;
        }
    }
    KelpFileInfo {
        state,
        changelist: r.opened.map_or(0, |(_, cl)| cl.0),
        needs_resolve: r.needs_resolve,
        exclusive: r.exclusive,
        locked: r.locked_by.is_some(),
        locked_here: r.locked_here,
        out_of_date: r.out_of_date,
        locked_by,
    }
}

/// Opens files for `action` in changelist `changelist`.
///
/// # Safety
///
/// `ws` must be a live handle and `paths` must point to `count` valid
/// NUL-terminated UTF-8 strings.
unsafe fn open(
    ws: *const KelpWorkspace,
    paths: *const *const c_char,
    count: usize,
    changelist: u32,
    action: OpenAction,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    let Some(ws) = (unsafe { ws.as_ref() }) else {
        return fail("null workspace");
    };
    let paths = match unsafe { read_paths(ws, paths, count) } {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    match ws.run(Op::Open {
        paths,
        action,
        cl: LocalClId(changelist),
    }) {
        Ok(_) => KELP_OK,
        Err(e) => fail(e),
    }
}

/// Opens files for edit, taking exclusive locks where the typemap requires them.
///
/// # Safety
///
/// See [`kelp_file_info`]; `paths` must point to `count` valid strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kelp_edit(
    ws: *const KelpWorkspace,
    paths: *const *const c_char,
    count: usize,
    changelist: u32,
) -> i32 {
    guard(|| {
        // SAFETY: forwarded from the caller's guarantees.
        unsafe { open(ws, paths, count, changelist, OpenAction::Edit) }
    })
}

/// Opens new files for add.
///
/// # Safety
///
/// See [`kelp_edit`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kelp_add(
    ws: *const KelpWorkspace,
    paths: *const *const c_char,
    count: usize,
    changelist: u32,
) -> i32 {
    guard(|| {
        // SAFETY: forwarded from the caller's guarantees.
        unsafe { open(ws, paths, count, changelist, OpenAction::Add) }
    })
}

/// Opens files for delete and removes them from disk.
///
/// # Safety
///
/// See [`kelp_edit`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kelp_delete(
    ws: *const KelpWorkspace,
    paths: *const *const c_char,
    count: usize,
    changelist: u32,
) -> i32 {
    guard(|| {
        // SAFETY: forwarded from the caller's guarantees.
        unsafe { open(ws, paths, count, changelist, OpenAction::Delete) }
    })
}

/// Reverts opened files.
///
/// # Safety
///
/// See [`kelp_edit`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kelp_revert(
    ws: *const KelpWorkspace,
    paths: *const *const c_char,
    count: usize,
) -> i32 {
    guard(|| {
        // SAFETY: forwarded from the caller's guarantees.
        let Some(ws) = (unsafe { ws.as_ref() }) else {
            return fail("null workspace");
        };
        let paths = match unsafe { read_paths(ws, paths, count) } {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        match ws.run(Op::Revert { paths }) {
            Ok(_) => KELP_OK,
            Err(e) => fail(e),
        }
    })
}

/// Syncs the workspace to the head of its stream and stores the changelist
/// number in `out_cl` if it is not null.
///
/// # Safety
///
/// `ws` must be a live handle; `out_cl` must be null or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kelp_sync(ws: *const KelpWorkspace, out_cl: *mut u64) -> i32 {
    guard(|| {
        // SAFETY: forwarded from the caller's guarantees.
        let Some(ws) = (unsafe { ws.as_ref() }) else {
            return fail("null workspace");
        };
        match ws.run(Op::Sync { at: None }) {
            Ok(Outcome::Synced(report)) => {
                if !out_cl.is_null() {
                    // SAFETY: `out_cl` is non-null and valid per the caller.
                    unsafe { *out_cl = report.cl };
                }
                KELP_OK
            }
            Ok(_) => fail("unexpected reply"),
            Err(e) => fail(e),
        }
    })
}

/// Submits changelist `changelist` with `description` (null keeps the
/// changelist's own) and stores the new changelist number in `out_cl` if it
/// is not null.
///
/// # Safety
///
/// `ws` must be a live handle; `description` must be null or a NUL-terminated
/// UTF-8 string; `out_cl` must be null or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kelp_submit(
    ws: *const KelpWorkspace,
    changelist: u32,
    description: *const c_char,
    out_cl: *mut u64,
) -> i32 {
    guard(|| {
        // SAFETY: forwarded from the caller's guarantees.
        let Some(ws) = (unsafe { ws.as_ref() }) else {
            return fail("null workspace");
        };
        let message = match description.is_null() {
            true => None,
            false => match unsafe { read_str(description) } {
                Ok(d) => Some(d.to_owned()),
                Err(e) => return fail(e),
            },
        };
        match ws.run(Op::Submit {
            cl: LocalClId(changelist),
            message,
            fixes: Vec::new(),
        }) {
            Ok(Outcome::Submitted(number)) => {
                if !out_cl.is_null() {
                    // SAFETY: `out_cl` is non-null and valid per the caller.
                    unsafe { *out_cl = number };
                }
                KELP_OK
            }
            Ok(_) => fail("unexpected reply"),
            Err(e) => fail(e),
        }
    })
}
