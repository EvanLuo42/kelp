use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::process::Command;

use kelp_client::ops::{Op, Session};
use kelp_core::LocalClId;
use kelp_proto::Request;
use kelp_sdk::*;
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};

const TYPEMAP: &str =
    "[[rule]]\npattern = \"**/*.uasset\"\nkind = \"binary\"\nlock = \"exclusive\"\n";

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(kelp_last_error()) }
        .to_string_lossy()
        .into_owned()
}

/// A server with depot `game`, an exclusive `Hero.uasset` and two users'
/// machines with synced workspaces.
async fn setup() -> (TestServer, TestClient, TestClient) {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    server
        .admin(Request::SetTypemap {
            depot: "game".into(),
            source: TYPEMAP.into(),
        })
        .await;
    let (alice, bob) = (TestClient::new(), TestClient::new());
    for (client, user) in [(&alice, "alice"), (&bob, "bob")] {
        server.add_user(user).await;
        client.login(&server, user).await;
        client.create_workspace(&server, "ws", &depot_path("//game/main"));
    }
    alice.sandbox().write("ws/Hero.uasset", [1, 2, 3]);
    let session = Session::new(alice.home().clone());
    let cwd = alice.workspace_root("ws");
    session.execute(&cwd, Op::Reconcile).await.unwrap();
    session
        .execute(
            &cwd,
            Op::Submit {
                cl: LocalClId::DEFAULT,
                message: Some("hero".into()),
                fixes: Vec::new(),
            },
        )
        .await
        .unwrap();
    Session::new(bob.home().clone())
        .execute(&bob.workspace_root("ws"), Op::Sync { at: None })
        .await
        .unwrap();
    (server, alice, bob)
}

fn open(client: &TestClient) -> *mut KelpWorkspace {
    let root = c(client.workspace_root("ws").to_str().unwrap());
    let home = c(client.home().dir().to_str().unwrap());
    let ws = unsafe { kelp_workspace_open(root.as_ptr(), home.as_ptr()) };
    assert!(!ws.is_null(), "{}", last_error());
    ws
}

fn info(ws: *const KelpWorkspace, path: &str) -> KelpFileInfo {
    let mut out = std::mem::MaybeUninit::<KelpFileInfo>::uninit();
    let path = c(path);
    assert_eq!(
        unsafe { kelp_file_info(ws, path.as_ptr(), out.as_mut_ptr()) },
        KELP_OK,
        "{}",
        last_error()
    );
    unsafe { out.assume_init() }
}

#[tokio::test(flavor = "multi_thread")]
async fn editors_see_locks_and_check_out_files() {
    let (_server, alice, bob) = setup().await;
    let (wa, wb) = (open(&alice), open(&bob));
    assert_eq!(kelp_abi_version(), 1);

    let before = info(wb, "Hero.uasset");
    assert_eq!(before.state, KelpFileState::Synced);
    assert!(before.exclusive && !before.locked);

    let hero = c("Hero.uasset");
    let paths = [hero.as_ptr()];
    assert_eq!(
        unsafe { kelp_edit(wb, paths.as_ptr(), 1, 0) },
        KELP_OK,
        "{}",
        last_error()
    );
    let mine = info(wb, "Hero.uasset");
    assert_eq!(mine.state, KelpFileState::OpenedForEdit);
    assert!(mine.locked_here);

    let theirs = info(wa, "Hero.uasset");
    assert!(theirs.locked && !theirs.locked_here);
    let holder = unsafe { CStr::from_ptr(theirs.locked_by.as_ptr()) }
        .to_str()
        .unwrap();
    assert_eq!(holder, "bob@ws");
    assert_eq!(unsafe { kelp_edit(wa, paths.as_ptr(), 1, 0) }, KELP_ERROR);
    assert!(
        last_error().contains("locked by bob@ws"),
        "{}",
        last_error()
    );

    bob.sandbox().write("ws/Hero.uasset", [9, 9]);
    let message = c("new hero");
    let mut cl = 0u64;
    assert_eq!(
        unsafe { kelp_submit(wb, 0, message.as_ptr(), &mut cl) },
        KELP_OK,
        "{}",
        last_error()
    );
    assert_eq!(cl, 2);
    assert!(info(wa, "Hero.uasset").out_of_date);
    assert_eq!(
        unsafe { kelp_sync(wa, &mut cl) },
        KELP_OK,
        "{}",
        last_error()
    );
    assert!(!info(wa, "Hero.uasset").out_of_date);

    unsafe {
        kelp_workspace_close(wa);
        kelp_workspace_close(wb);
    }
}

#[test]
fn errors_are_reported_per_thread() {
    let missing = c("/definitely/not/a/workspace");
    let home = c("/tmp");
    assert!(unsafe { kelp_workspace_open(missing.as_ptr(), home.as_ptr()) }.is_null());
    assert!(last_error().contains("does not exist"), "{}", last_error());
    let other = std::thread::spawn(|| {
        unsafe { CStr::from_ptr(kelp_last_error()) }
            .to_bytes()
            .is_empty()
    });
    assert!(other.join().unwrap());
    assert!(
        unsafe { kelp_file_info(std::ptr::null(), std::ptr::null(), std::ptr::null_mut()) }
            == KELP_ERROR
    );
}

/// Compiles `examples/checkout.c` against the shared library and runs it.
#[tokio::test(flavor = "multi_thread")]
async fn the_c_example_checks_out_a_file() {
    // The library lookup and linker flags below are ELF-specific; Windows
    // runners have gcc but produce kelp_sdk.dll instead of libkelp_sdk.so.
    if !cfg!(target_os = "linux") {
        eprintln!("the C example is only built on Linux; skipping");
        return;
    }
    let Some(cc) = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|cc| Command::new(cc).arg("--version").output().is_ok())
    else {
        eprintln!("no C compiler; skipping");
        return;
    };
    let (_server, alice, _bob) = setup().await;
    // `cargo test` leaves the shared library in target/<profile>/deps, next to
    // this test binary; `cargo build` also copies it one level up.
    let deps = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let lib_dir = [deps.clone(), deps.parent().unwrap().to_owned()]
        .into_iter()
        .find(|d| d.join("libkelp_sdk.so").exists())
        .expect("the kelp_sdk shared library is built");
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let exe = alice.sandbox().path("checkout");
    let status = Command::new(cc)
        .arg(crate_dir.join("examples/checkout.c"))
        .arg("-I")
        .arg(crate_dir.join("include"))
        .arg("-L")
        .arg(&lib_dir)
        .arg("-lkelp_sdk")
        .arg("-o")
        .arg(&exe)
        .status()
        .unwrap();
    assert!(status.success(), "compiling the C example failed");

    let workspace = alice.workspace_root("ws");
    let home = alice.home().dir().to_owned();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(exe)
            .arg(workspace)
            .arg(home)
            .arg("Hero.uasset")
            .env("LD_LIBRARY_PATH", lib_dir)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("state=synced exclusive=1 locked_here=0"),
        "{stdout}"
    );
    assert!(
        stdout.contains("state=opened-for-edit exclusive=1 locked_here=1 locked_by=alice@ws"),
        "{stdout}"
    );
}
