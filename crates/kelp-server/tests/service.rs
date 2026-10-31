use std::path::PathBuf;
use std::sync::Mutex;

use kelp_server::service::{
    EdgeSpec, Scope, ServiceSpec, Systemctl, install, uninstall, unit_file,
};
use kelp_testkit::Sandbox;

/// Records the `systemctl` calls instead of making them.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
}

impl Systemctl for Recorder {
    fn run(&self, scope: Scope, args: &[&str]) -> kelp_server::Result<()> {
        let prefix = if scope == Scope::User { "--user " } else { "" };
        self.calls
            .lock()
            .unwrap()
            .push(format!("{prefix}{}", args.join(" ")));
        Ok(())
    }
}

fn spec(scope: Scope) -> ServiceSpec {
    ServiceSpec {
        name: "kelp-server".into(),
        scope,
        binary: PathBuf::from("/usr/local/bin/kelp-server"),
        data_dir: PathBuf::from("/srv/kelp data/100%"),
        listen: "[::]:7474".parse().unwrap(),
        run_as: Some("kelp".into()),
        edge: None,
    }
}

#[test]
fn a_system_unit_runs_the_server_at_boot_as_its_account() {
    let unit = unit_file(&spec(Scope::System));
    assert!(unit.contains(
        "ExecStart=/usr/local/bin/kelp-server --data \"/srv/kelp data/100%%\" serve --listen [::]:7474\n"
    ), "{unit}");
    assert!(unit.contains("User=kelp\n"));
    assert!(unit.contains("Restart=on-failure\n"));
    assert!(
        unit.contains("KillSignal=SIGINT\n"),
        "the server shuts down cleanly on SIGINT"
    );
    assert!(unit.contains("WantedBy=multi-user.target\n"));
    assert!(unit.contains("After=network-online.target\n"));
}

#[test]
fn a_user_unit_runs_as_the_user_and_an_edge_unit_names_its_commit_server() {
    let mut edge = spec(Scope::User);
    edge.edge = Some(EdgeSpec {
        upstream: "commit.studio:7474".into(),
        fingerprint: "ab12".into(),
        token_file: PathBuf::from("/srv/edge.token"),
    });
    let unit = unit_file(&edge);
    assert!(!unit.contains("User="), "user units run as their owner");
    assert!(unit.contains("WantedBy=default.target\n"));
    assert!(unit.contains("Description=Kelp edge"));
    assert!(unit.contains(
        " --upstream commit.studio:7474 --upstream-fingerprint ab12 --token-file /srv/edge.token\n"
    ), "{unit}");
}

#[test]
fn install_and_uninstall_manage_the_unit_and_systemd() {
    let sandbox = Sandbox::new();
    let dir = sandbox.path("units");
    let systemctl = Recorder::default();
    let path = install(&spec(Scope::User), &dir, &systemctl).unwrap();
    assert_eq!(path, dir.join("kelp-server.service"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        unit_file(&spec(Scope::User))
    );
    uninstall("kelp-server", Scope::User, &dir, &systemctl).unwrap();
    assert!(!path.exists());
    assert_eq!(
        *systemctl.calls.lock().unwrap(),
        [
            "--user daemon-reload",
            "--user enable --now kelp-server.service",
            "--user disable --now kelp-server.service",
            "--user daemon-reload",
        ]
    );
    assert!(uninstall("kelp-server", Scope::User, &dir, &systemctl).is_err());
}
