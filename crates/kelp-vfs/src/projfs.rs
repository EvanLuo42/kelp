//! The ProjFS presentation of a projected workspace on Windows.
//!
//! The virtualization root is the workspace itself: files that were opened
//! become real files there, and ProjFS keeps tombstones for deleted ones, so
//! no separate upper directory is needed.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use windows::Win32::Foundation::{
    BOOLEAN, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_PATH_NOT_FOUND,
};
use windows::Win32::Storage::ProjectedFileSystem::*;
use windows::core::{GUID, HRESULT, HSTRING, PCWSTR, w};

use crate::{Hydrator, LowerFile, LowerTree, MountConfig, Mounted, Projection};

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// The difference between the Windows and Unix epochs, in 100 ns intervals.
const EPOCH_DIFFERENCE: i64 = 116_444_736_000_000_000;
const WRITE_CHUNK: usize = 4 << 20;
const S_OK: HRESULT = HRESULT(0);
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x800;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: PCWSTR, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(
        module: *mut c_void,
        name: *const u8,
    ) -> Option<unsafe extern "system" fn() -> isize>;
}

/// Declares [`Api`], the ProjFS functions this module calls.
macro_rules! api {
    ($($name:ident($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        /// ProjFS functions, looked up when first needed rather than linked:
        /// ProjFS is an optional Windows feature, and a program that imports
        /// a missing DLL does not start at all.
        #[allow(non_snake_case)]
        struct Api {
            $($name: unsafe extern "system" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Api {
            fn load() -> Option<Self> {
                // SAFETY: the name is a valid wide string, and the search is
                // limited to System32 so no planted DLL is picked up.
                let module = unsafe {
                    LoadLibraryExW(w!("ProjectedFSLib.dll"), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32)
                };
                if module.is_null() {
                    return None;
                }
                Some(Self {
                    $($name: {
                        // SAFETY: the module stays loaded for the life of the
                        // process, and the signature is the documented one.
                        let f = unsafe {
                            GetProcAddress(module, concat!(stringify!($name), "\0").as_ptr())
                        }?;
                        unsafe {
                            std::mem::transmute::<
                                unsafe extern "system" fn() -> isize,
                                unsafe extern "system" fn($($arg),*) $(-> $ret)?,
                            >(f)
                        }
                    },)*
                })
            }
        }
    };
}

api! {
    PrjAllocateAlignedBuffer(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, usize) -> *mut c_void;
    PrjClearNegativePathCache(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, *mut u32) -> HRESULT;
    PrjDeleteFile(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, PCWSTR, PRJ_UPDATE_TYPES, *mut PRJ_UPDATE_FAILURE_CAUSES) -> HRESULT;
    PrjFileNameCompare(PCWSTR, PCWSTR) -> i32;
    PrjFileNameMatch(PCWSTR, PCWSTR) -> BOOLEAN;
    PrjFillDirEntryBuffer(PCWSTR, *const PRJ_FILE_BASIC_INFO, PRJ_DIR_ENTRY_BUFFER_HANDLE) -> HRESULT;
    PrjFreeAlignedBuffer(*const c_void);
    PrjMarkDirectoryAsPlaceholder(PCWSTR, PCWSTR, *const PRJ_PLACEHOLDER_VERSION_INFO, *const GUID) -> HRESULT;
    PrjStartVirtualizing(PCWSTR, *const PRJ_CALLBACKS, *const c_void, *const PRJ_STARTVIRTUALIZING_OPTIONS, *mut PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT) -> HRESULT;
    PrjStopVirtualizing(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT);
    PrjUpdateFileIfNeeded(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, PCWSTR, *const PRJ_PLACEHOLDER_INFO, u32, PRJ_UPDATE_TYPES, *mut PRJ_UPDATE_FAILURE_CAUSES) -> HRESULT;
    PrjWriteFileData(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, *const GUID, *const c_void, u64, u32) -> HRESULT;
    PrjWritePlaceholderInfo(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, PCWSTR, *const PRJ_PLACEHOLDER_INFO, u32) -> HRESULT;
}

/// Returns the ProjFS functions, or an error if ProjFS is not enabled.
fn api() -> io::Result<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(Api::load).as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows Projected File System is not enabled; enable the \
             Client-ProjFS optional feature to use projected workspaces",
        )
    })
}

fn pcwstr(s: &HSTRING) -> PCWSTR {
    PCWSTR(s.as_ptr())
}

/// A ProjFS virtualization context, which may be used from any thread.
#[derive(Clone, Copy)]
struct Context(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT);

// SAFETY: ProjFS contexts are opaque handles that the API allows on any thread.
unsafe impl Send for Context {}
// SAFETY: as above.
unsafe impl Sync for Context {}

struct Enumeration {
    entries: Vec<(String, Option<LowerFile>)>,
    next: usize,
    pattern: Option<HSTRING>,
}

struct Inner {
    api: &'static Api,
    lower: RwLock<LowerTree>,
    hydrator: Arc<dyn Hydrator>,
    enumerations: Mutex<HashMap<u128, Enumeration>>,
    context: Mutex<Option<Context>>,
}

struct Session(Arc<Inner>);

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(Context(ctx)) = self.0.context.lock().expect("context lock").take() {
            // SAFETY: the context came from PrjStartVirtualizing and is stopped once.
            unsafe { (self.0.api.PrjStopVirtualizing)(ctx) };
        }
    }
}

pub(crate) fn mount(config: MountConfig) -> io::Result<Mounted> {
    let api = api()?;
    let root = config.mountpoint;
    let first_mount = !root.join(".kelp").join("projfs-id").exists();
    let instance = instance_id(&root)?;
    let inner = Arc::new(Inner {
        api,
        lower: RwLock::new(config.lower),
        hydrator: config.hydrator,
        enumerations: Mutex::default(),
        context: Mutex::default(),
    });
    let callbacks = PRJ_CALLBACKS {
        StartDirectoryEnumerationCallback: Some(start_enumeration),
        EndDirectoryEnumerationCallback: Some(end_enumeration),
        GetDirectoryEnumerationCallback: Some(get_enumeration),
        GetPlaceholderInfoCallback: Some(get_placeholder_info),
        GetFileDataCallback: Some(get_file_data),
        ..Default::default()
    };
    let root_name = HSTRING::from(root.as_os_str());
    // SAFETY: the callbacks match the ProjFS signatures, and `inner` outlives
    // the virtualization because the returned session stops it before
    // dropping its Arc.
    let ctx = unsafe {
        if first_mount {
            (api.PrjMarkDirectoryAsPlaceholder)(
                pcwstr(&root_name),
                PCWSTR::null(),
                null(),
                &instance,
            )
            .ok()
            .map_err(io::Error::other)?;
        }
        let mut ctx = PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT(null_mut());
        (api.PrjStartVirtualizing)(
            pcwstr(&root_name),
            &callbacks,
            Arc::as_ptr(&inner) as *const c_void,
            null(),
            &mut ctx,
        )
        .ok()
        .map_err(io::Error::other)?;
        ctx
    };
    *inner.context.lock().expect("context lock") = Some(Context(ctx));
    let projection = Arc::new(ProjFs(inner.clone()));
    Ok(Mounted {
        projection,
        _session: Box::new(Session(inner)),
    })
}

/// Returns the virtualization instance id of `root`, creating and saving one
/// on first use.
fn instance_id(root: &Path) -> io::Result<GUID> {
    let path: PathBuf = root.join(".kelp").join("projfs-id");
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(bytes) = <[u8; 16]>::try_from(bytes.as_slice())
    {
        return Ok(GUID::from_u128(u128::from_le_bytes(bytes)));
    }
    let id = GUID::new().map_err(io::Error::other)?;
    std::fs::create_dir_all(root.join(".kelp"))?;
    std::fs::write(&path, id.to_u128().to_le_bytes())?;
    Ok(id)
}

struct ProjFs(Arc<Inner>);

impl Projection for ProjFs {
    fn project(&self, local: &str, file: Option<LowerFile>) -> io::Result<bool> {
        let set_lower = || {
            let mut lower = self.0.lower.write().expect("lower lock");
            match file {
                Some(f) => lower.insert(local, f),
                None => lower.remove(local),
            }
        };
        let Some(Context(ctx)) = *self.0.context.lock().expect("context lock") else {
            set_lower();
            return Ok(true);
        };
        let name = HSTRING::from(local.replace('/', "\\"));
        // Without PRJ_UPDATE_ALLOW_DIRTY_DATA, ProjFS refuses to replace or
        // delete a file someone changed, which is what keeps their work.
        let flags = PRJ_UPDATE_ALLOW_DIRTY_METADATA
            | PRJ_UPDATE_ALLOW_TOMBSTONE
            | PRJ_UPDATE_ALLOW_READ_ONLY;
        let mut cause = PRJ_UPDATE_FAILURE_CAUSES(0);
        // SAFETY: `ctx` is a live context and the placeholder info is fully initialised.
        let api = self.0.api;
        let result = unsafe {
            match file {
                Some(f) => {
                    let info = placeholder_info(Some(&f));
                    let size = std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32;
                    let updated = (api.PrjUpdateFileIfNeeded)(
                        ctx,
                        pcwstr(&name),
                        &info,
                        size,
                        flags,
                        &mut cause,
                    );
                    let _ = (api.PrjClearNegativePathCache)(ctx, null_mut());
                    updated
                }
                None => (api.PrjDeleteFile)(ctx, pcwstr(&name), flags, &mut cause),
            }
        }
        .ok();
        match result {
            Ok(()) => {
                set_lower();
                Ok(true)
            }
            Err(e)
                if e.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                    || e.code() == ERROR_PATH_NOT_FOUND.to_hresult() =>
            {
                set_lower();
                Ok(true)
            }
            Err(_) if cause.0 & PRJ_UPDATE_FAILURE_CAUSE_DIRTY_DATA.0 != 0 => Ok(false),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}

/// Returns the context passed to PrjStartVirtualizing.
///
/// # Safety
///
/// `data` must be the callback data ProjFS passed to a callback of an
/// instance started by [`mount`].
unsafe fn inner<'a>(data: *const PRJ_CALLBACK_DATA) -> &'a Inner {
    // SAFETY: the instance context is the pointer to the Inner kept alive by the session.
    unsafe { &*((*data).InstanceContext as *const Inner) }
}

/// Returns the callback's path in workspace form, with `/` separators.
///
/// # Safety
///
/// As for [`inner`].
unsafe fn path_of(data: *const PRJ_CALLBACK_DATA) -> String {
    // SAFETY: FilePathName is a NUL-terminated string valid during the callback.
    unsafe { (*data).FilePathName.to_string() }
        .unwrap_or_default()
        .replace('\\', "/")
}

/// Finds the stored spelling of `path`, which ProjFS passes case-insensitively.
fn resolve(lower: &LowerTree, path: &str) -> Option<String> {
    let mut actual = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        let folded = part.to_lowercase();
        let name = lower
            .children(&actual)
            .find(|c| c.to_lowercase() == folded)?;
        actual = if actual.is_empty() {
            name.to_owned()
        } else {
            format!("{actual}/{name}")
        };
    }
    Some(actual)
}

fn filetime(nanos: u64) -> i64 {
    (nanos / 100) as i64 + EPOCH_DIFFERENCE
}

fn basic_info(file: Option<&LowerFile>) -> PRJ_FILE_BASIC_INFO {
    match file {
        Some(f) => PRJ_FILE_BASIC_INFO {
            IsDirectory: false.into(),
            FileSize: f.size as i64,
            CreationTime: filetime(f.mtime),
            LastAccessTime: filetime(f.mtime),
            LastWriteTime: filetime(f.mtime),
            ChangeTime: filetime(f.mtime),
            FileAttributes: if f.read_only {
                FILE_ATTRIBUTE_READONLY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
        },
        None => PRJ_FILE_BASIC_INFO {
            IsDirectory: true.into(),
            FileAttributes: FILE_ATTRIBUTE_DIRECTORY,
            ..Default::default()
        },
    }
}

fn placeholder_info(file: Option<&LowerFile>) -> PRJ_PLACEHOLDER_INFO {
    PRJ_PLACEHOLDER_INFO {
        FileBasicInfo: basic_info(file),
        ..Default::default()
    }
}

unsafe extern "system" fn start_enumeration(
    data: *const PRJ_CALLBACK_DATA,
    id: *const GUID,
) -> HRESULT {
    // SAFETY: ProjFS passes valid callback data and enumeration id.
    let (inner, dir, id) = unsafe { (inner(data), path_of(data), (*id).to_u128()) };
    let lower = inner.lower.read().expect("lower lock");
    let Some(dir) = resolve(&lower, &dir) else {
        return ERROR_FILE_NOT_FOUND.to_hresult();
    };
    let mut entries: Vec<(String, Option<LowerFile>)> = lower
        .children(&dir)
        .map(|name| {
            let local = if dir.is_empty() {
                name.to_owned()
            } else {
                format!("{dir}/{name}")
            };
            (name.to_owned(), lower.file(&local).copied())
        })
        .collect();
    drop(lower);
    entries.sort_by(|(a, _), (b, _)| {
        // SAFETY: both names are valid NUL-terminated wide strings.
        unsafe {
            (inner.api.PrjFileNameCompare)(pcwstr(&HSTRING::from(a)), pcwstr(&HSTRING::from(b)))
        }
        .cmp(&0)
    });
    inner.enumerations.lock().expect("enumeration lock").insert(
        id,
        Enumeration {
            entries,
            next: 0,
            pattern: None,
        },
    );
    S_OK
}

unsafe extern "system" fn end_enumeration(
    data: *const PRJ_CALLBACK_DATA,
    id: *const GUID,
) -> HRESULT {
    // SAFETY: ProjFS passes valid callback data and enumeration id.
    let (inner, id) = unsafe { (inner(data), (*id).to_u128()) };
    inner
        .enumerations
        .lock()
        .expect("enumeration lock")
        .remove(&id);
    S_OK
}

unsafe extern "system" fn get_enumeration(
    data: *const PRJ_CALLBACK_DATA,
    id: *const GUID,
    search: PCWSTR,
    buffer: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    // SAFETY: ProjFS passes valid callback data, enumeration id and buffer.
    let (inner, id, restart) = unsafe {
        (
            inner(data),
            (*id).to_u128(),
            (*data).Flags.0 & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN.0 != 0,
        )
    };
    let mut enumerations = inner.enumerations.lock().expect("enumeration lock");
    let Some(e) = enumerations.get_mut(&id) else {
        return ERROR_FILE_NOT_FOUND.to_hresult();
    };
    if restart {
        e.next = 0;
        e.pattern = None;
    }
    if e.pattern.is_none() && !search.is_null() {
        // SAFETY: a non-null search expression is a valid wide string.
        e.pattern = unsafe { search.to_string() }.ok().map(HSTRING::from);
    }
    while let Some((name, file)) = e.entries.get(e.next) {
        let wide = HSTRING::from(name.as_str());
        // SAFETY: the strings are valid and the buffer handle comes from ProjFS.
        let matches = e.pattern.as_ref().is_none_or(|p| {
            unsafe { (inner.api.PrjFileNameMatch)(pcwstr(&wide), pcwstr(p)) }.as_bool()
        });
        if matches {
            let info = basic_info(file.as_ref());
            // SAFETY: as above.
            if let Err(err) =
                unsafe { (inner.api.PrjFillDirEntryBuffer)(pcwstr(&wide), &info, buffer) }.ok()
            {
                if err.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() {
                    break;
                }
                return err.code();
            }
        }
        e.next += 1;
    }
    S_OK
}

unsafe extern "system" fn get_placeholder_info(data: *const PRJ_CALLBACK_DATA) -> HRESULT {
    // SAFETY: ProjFS passes valid callback data.
    let (inner, path) = unsafe { (inner(data), path_of(data)) };
    let lower = inner.lower.read().expect("lower lock");
    let Some(actual) = resolve(&lower, &path) else {
        return ERROR_FILE_NOT_FOUND.to_hresult();
    };
    let file = lower.file(&actual).copied();
    drop(lower);
    let info = placeholder_info(file.as_ref());
    let name = HSTRING::from(actual.replace('/', "\\"));
    let size = std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32;
    // SAFETY: the context comes from the callback data and the info is initialised.
    match unsafe {
        (inner.api.PrjWritePlaceholderInfo)(
            (*data).NamespaceVirtualizationContext,
            pcwstr(&name),
            &info,
            size,
        )
    }
    .ok()
    {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

unsafe extern "system" fn get_file_data(
    data: *const PRJ_CALLBACK_DATA,
    offset: u64,
    length: u32,
) -> HRESULT {
    // SAFETY: ProjFS passes valid callback data.
    let (inner, path) = unsafe { (inner(data), path_of(data)) };
    let lower = inner.lower.read().expect("lower lock");
    let Some((actual, file)) =
        resolve(&lower, &path).and_then(|a| lower.file(&a).copied().map(|f| (a, f)))
    else {
        return ERROR_FILE_NOT_FOUND.to_hresult();
    };
    drop(lower);
    let mut content = Vec::with_capacity(file.size as usize);
    if inner
        .hydrator
        .hydrate(&actual, &file, &mut content)
        .is_err()
    {
        return ERROR_FILE_NOT_FOUND.to_hresult();
    }
    let end = (offset + u64::from(length)).min(content.len() as u64);
    let mut position = offset;
    while position < end {
        let chunk = ((end - position) as usize).min(WRITE_CHUNK);
        // SAFETY: the buffer is allocated for this context with `chunk` bytes,
        // filled from `content`, written and freed.
        let result = unsafe {
            let ctx = (*data).NamespaceVirtualizationContext;
            let api = inner.api;
            let buffer = (api.PrjAllocateAlignedBuffer)(ctx, chunk);
            if buffer.is_null() {
                return windows::Win32::Foundation::E_OUTOFMEMORY;
            }
            std::ptr::copy_nonoverlapping(
                content.as_ptr().add(position as usize),
                buffer as *mut u8,
                chunk,
            );
            let written =
                (api.PrjWriteFileData)(ctx, &(*data).DataStreamId, buffer, position, chunk as u32);
            (api.PrjFreeAlignedBuffer)(buffer);
            written.ok()
        };
        if let Err(e) = result {
            return e.code();
        }
        position += chunk as u64;
    }
    S_OK
}
