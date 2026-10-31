//! Mounts a real FUSE file system; skipped where FUSE is unavailable.

#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;

use kelp_testkit::Sandbox;
use kelp_vfs::{MountConfig, Mounted};

fn mount(sandbox: &Sandbox) -> Option<(Mounted, Arc<common::MemoryHydrator>)> {
    let (lower, hydrator) = common::sample();
    let hydrator = Arc::new(hydrator);
    let config = MountConfig {
        mountpoint: sandbox.dir("mnt"),
        upper: sandbox.dir("upper"),
        lower,
        hydrator: hydrator.clone(),
    };
    match kelp_vfs::mount(config) {
        Ok(m) => Some((m, hydrator)),
        Err(e) => {
            eprintln!("FUSE unavailable ({e}); skipping");
            None
        }
    }
}

#[test]
fn programs_see_lazy_files_through_the_mount() {
    let sandbox = Sandbox::new();
    let Some((mounted, hydrator)) = mount(&sandbox) else {
        return;
    };
    let mnt = sandbox.path("mnt");

    let mut names: Vec<_> = std::fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["Content", "a.txt"]);
    assert_eq!(
        std::fs::metadata(mnt.join("Content/Hero.png"))
            .unwrap()
            .len(),
        3
    );
    assert!(hydrator.hydrated.lock().unwrap().is_empty());

    assert_eq!(
        std::fs::read(mnt.join("Content/Maps/level.map")).unwrap(),
        b"map"
    );
    assert_eq!(
        *hydrator.hydrated.lock().unwrap(),
        ["Content/Maps/level.map"]
    );

    std::fs::write(mnt.join("a.txt"), "changed").unwrap();
    assert_eq!(
        std::fs::read_to_string(sandbox.path("upper/a.txt")).unwrap(),
        "changed"
    );
    std::fs::create_dir_all(mnt.join("New/Dir")).unwrap();
    std::fs::write(mnt.join("New/Dir/f.txt"), "f").unwrap();
    std::fs::remove_file(mnt.join("Content/Hero.png")).unwrap();
    assert!(!mnt.join("Content/Hero.png").exists());
    std::fs::rename(mnt.join("New"), mnt.join("Old")).unwrap();
    assert_eq!(
        std::fs::read_to_string(mnt.join("Old/Dir/f.txt")).unwrap(),
        "f"
    );

    let synced = common::lower_file(&mut common::MemoryHydrator::default(), b"synced");
    mounted
        .projection()
        .project("Synced.txt", Some(synced))
        .unwrap();
    assert_eq!(
        std::fs::metadata(mnt.join("Synced.txt")).unwrap().len(),
        6,
        "sync shows files without content"
    );

    drop(mounted);
    assert!(
        std::fs::read_dir(&mnt).unwrap().next().is_none(),
        "unmounted on drop"
    );
}

#[test]
fn a_rename_that_must_not_replace_does_not() {
    let sandbox = Sandbox::new();
    let Some((_mounted, _)) = mount(&sandbox) else {
        return;
    };
    let mnt = sandbox.path("mnt");
    std::fs::write(mnt.join("b.txt"), "keep me").unwrap();
    let path = |name: &str| std::ffi::CString::new(mnt.join(name).to_str().unwrap()).unwrap();
    let (from, to) = (path("a.txt"), path("b.txt"));
    // SAFETY: both paths are valid NUL-terminated strings.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    assert_eq!(result, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EEXIST)
    );
    assert_eq!(std::fs::read(mnt.join("b.txt")).unwrap(), b"keep me");
    assert_eq!(std::fs::read(mnt.join("a.txt")).unwrap(), b"alpha");
}
