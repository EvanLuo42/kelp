use kelp_meta::db::webhooks::MAX_ATTEMPTS;
use kelp_meta::{Command, Governance, MetaDb};
use kelp_testkit::Sandbox;
use kelp_testkit::meta::MetaFixture;

fn hook(f: &MetaFixture, events: &[&str], paths: &[&str]) {
    f.db()
        .commit(Governance::SetWebhook {
            name: "ci".into(),
            url: "https://ci.example/kelp".into(),
            secret: "s3cret".into(),
            events: events.iter().map(|e| e.to_string()).collect(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
        })
        .unwrap();
}

#[test]
fn events_are_queued_for_matching_webhooks() {
    let f = MetaFixture::new();
    hook(&f, &["submit"], &["//game/main/Code/..."]);
    f.submit("//game/main", vec![f.add("//game/main/readme.txt", b"hi")])
        .unwrap();
    let cl = f
        .submit("//game/main", vec![f.add("//game/main/Code/a.rs", b"a")])
        .unwrap();
    f.db()
        .commit(Command::CreateDepot {
            name: "tools".into(),
            at: 3,
        })
        .unwrap();

    let r = f.db().read().unwrap();
    let due = r.due_deliveries(0, 10).unwrap();
    assert_eq!(due.len(), 1, "only the submit to Code/ matches");
    let (delivery, webhook) = &due[0];
    assert_eq!(webhook.secret, "s3cret");
    let body: serde_json::Value = serde_json::from_slice(&delivery.body).unwrap();
    assert_eq!(body["event"], "submit");
    assert_eq!(body["actor"], "alice");
    assert_eq!(body["data"]["cl"], cl.0);
    assert_eq!(body["data"]["files"][0]["path"], "//game/main/Code/a.rs");
    let audit = r.audit(0, 100).unwrap();
    assert!(audit.iter().all(|e| !e.summary.contains("s3cret")));
}

#[test]
fn failed_deliveries_back_off_and_give_up() {
    let f = MetaFixture::new();
    hook(&f, &[], &[]);
    f.submit("//game/main", vec![f.add("//game/main/a.txt", b"a")])
        .unwrap();
    let id = f.db().read().unwrap().due_deliveries(0, 10).unwrap()[0]
        .0
        .id;

    f.db()
        .record_delivery(id, Err("500".into()), 1_000, 100)
        .unwrap();
    let r = f.db().read().unwrap();
    assert!(r.due_deliveries(1_099, 10).unwrap().is_empty());
    assert_eq!(r.due_deliveries(1_100, 10).unwrap().len(), 1);
    drop(r);
    f.db()
        .record_delivery(id, Err("500".into()), 2_000, 100)
        .unwrap();
    assert!(
        f.db()
            .read()
            .unwrap()
            .due_deliveries(2_199, 10)
            .unwrap()
            .is_empty(),
        "the wait doubles"
    );

    for _ in 2..MAX_ATTEMPTS {
        f.db()
            .record_delivery(id, Err("timeout".into()), 0, 0)
            .unwrap();
    }
    let r = f.db().read().unwrap();
    assert!(
        r.due_deliveries(u64::MAX, 10).unwrap().is_empty(),
        "given up"
    );
    let d = &r.deliveries(Some("ci"), 10).unwrap()[0];
    assert_eq!(
        (d.attempts, d.delivered, d.last_result.as_deref()),
        (MAX_ATTEMPTS, false, Some("timeout"))
    );
    drop(r);

    f.db().redeliver(id).unwrap();
    f.db()
        .record_delivery(id, Ok("200".into()), 5, 100)
        .unwrap();
    let r = f.db().read().unwrap();
    assert!(r.deliveries(None, 10).unwrap()[0].delivered);
    assert!(r.due_deliveries(u64::MAX, 10).unwrap().is_empty());
}

#[test]
fn replicas_do_not_deliver() {
    let f = MetaFixture::new();
    hook(&f, &[], &[]);
    f.submit("//game/main", vec![f.add("//game/main/a.txt", b"a")])
        .unwrap();
    let sandbox = Sandbox::new();
    let replica = MetaDb::open(&sandbox.path("replica.redb")).unwrap();
    for entry in f.db().read().unwrap().log_entries(1, 100).unwrap() {
        replica.replicate(&entry).unwrap();
    }
    let r = replica.read().unwrap();
    assert_eq!(r.webhooks().unwrap().len(), 1);
    assert!(r.deliveries(None, 10).unwrap().is_empty());
}
