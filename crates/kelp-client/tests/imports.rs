use kelp_client::{Connection, Error, Workspace, is_read_only};
use kelp_core::LocalClId;
use kelp_proto::rpc::ImportInfo;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

const TYPEMAP: &str = "[[rule]]\npattern = \"**/*.png\"\nkind = \"binary\"\nlock = \"exclusive\"\n";

/// A server where `Engine/` of `//game/main` imports `Source/` of
/// `//engine/main`, which holds `Source/a.h` and `Source/logo.png`.
struct Studio {
    server: TestServer,
    conn: Connection,
    engine: TestClient,
    engine_ws: Workspace,
    game: TestClient,
    game_ws: Workspace,
}

impl Studio {
    async fn new(writable: bool) -> Self {
        let server = TestServer::start().await;
        server.create_depot("engine").await;
        server.create_depot("game").await;
        server
            .admin(Request::SetTypemap {
                depot: "engine".into(),
                source: TYPEMAP.into(),
            })
            .await;
        let conn = server.add_user("alice").await;
        let (engine, game) = (TestClient::new(), TestClient::new());
        let mut engine_ws = engine.create_workspace(&server, "ws", &depot_path("//engine/main"));
        let game_ws = game.create_workspace(&server, "ws", &depot_path("//game/main"));
        engine.sandbox().write("ws/Source/a.h", "v1");
        engine.sandbox().write("ws/Source/logo.png", [1, 2, 3]);
        engine_ws.reconcile(&conn).await.unwrap();
        engine_ws
            .submit(
                &conn,
                &engine.cache(),
                LocalClId::DEFAULT,
                Some("engine".into()),
            )
            .await
            .unwrap();
        server
            .admin(Request::SetImports {
                stream: "//game/main".into(),
                imports: vec![ImportInfo {
                    source: "//engine/main".into(),
                    from: "Source".into(),
                    to: "Engine".into(),
                    at: None,
                    writable,
                }],
            })
            .await;
        Self {
            server,
            conn,
            engine,
            engine_ws,
            game,
            game_ws,
        }
    }

    async fn sync_game(&mut self) {
        self.game_ws
            .sync(&self.conn, &self.game.cache(), None)
            .await
            .unwrap();
    }

    async fn submit_game(&mut self, description: &str) -> Result<(), Error> {
        self.game_ws
            .submit(
                &self.conn,
                &self.game.cache(),
                LocalClId::DEFAULT,
                Some(description.into()),
            )
            .await
            .map(|_| ())
    }
}

#[tokio::test]
async fn an_import_shows_the_source_and_follows_it() {
    let mut s = Studio::new(false).await;
    s.sync_game().await;
    assert_eq!(s.game.sandbox().read("ws/Engine/a.h"), "v1".as_bytes());
    assert!(!s.game.sandbox().exists("ws/Source/a.h"));

    s.engine.sandbox().write("ws/Source/a.h", "v2");
    s.engine_ws.reconcile(&s.conn).await.unwrap();
    s.engine_ws
        .submit(
            &s.conn,
            &s.engine.cache(),
            LocalClId::DEFAULT,
            Some("v2".into()),
        )
        .await
        .unwrap();
    s.sync_game().await;
    assert_eq!(s.game.sandbox().read("ws/Engine/a.h"), "v2".as_bytes());

    let Reply::Filelog(revs) = s
        .conn
        .call(&Request::Filelog {
            path: "//game/main/Engine/a.h".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(revs.len(), 2, "the history is the source's");
}

#[tokio::test]
async fn read_only_imports_reject_changes() {
    let mut s = Studio::new(false).await;
    s.sync_game().await;
    let a = depot_path("//game/main/Engine/a.h");
    s.game_ws
        .open_files(&s.conn, &[a], OpenAction::Edit, LocalClId::DEFAULT)
        .await
        .unwrap();
    s.game.sandbox().write("ws/Engine/a.h", "mine");
    let err = s.submit_game("nope").await.unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Invalid && e.message.contains("read-only")),
        "{err}"
    );
}

#[tokio::test]
async fn writable_imports_submit_to_the_source() {
    let mut s = Studio::new(true).await;
    s.sync_game().await;
    let a = depot_path("//game/main/Engine/a.h");
    s.game_ws
        .open_files(&s.conn, &[a], OpenAction::Edit, LocalClId::DEFAULT)
        .await
        .unwrap();
    s.game.sandbox().write("ws/Engine/a.h", "fixed in the game");
    s.game.sandbox().write("ws/Code/main.cpp", "uses the fix");
    s.game_ws.reconcile(&s.conn).await.unwrap();
    s.submit_game("fix").await.unwrap();

    s.engine_ws
        .sync(&s.conn, &s.engine.cache(), None)
        .await
        .unwrap();
    assert_eq!(
        s.engine.sandbox().read("ws/Source/a.h"),
        "fixed in the game".as_bytes()
    );
    assert!(!s.engine.sandbox().exists("ws/Code/main.cpp"));

    let fresh = TestClient::new();
    let mut ws = fresh.create_workspace(&s.server, "ws", &depot_path("//game/main"));
    ws.sync(&s.conn, &fresh.cache(), None).await.unwrap();
    assert_eq!(
        fresh.sandbox().read("ws/Engine/a.h"),
        "fixed in the game".as_bytes()
    );
    assert_eq!(
        fresh.sandbox().read("ws/Code/main.cpp"),
        "uses the fix".as_bytes()
    );
}

#[tokio::test]
async fn imported_files_take_their_type_and_locks_from_the_source() {
    let mut s = Studio::new(true).await;
    s.sync_game().await;
    assert!(
        is_read_only(&s.game.sandbox().path("ws/Engine/logo.png")).unwrap(),
        "exclusive under the engine's typemap"
    );
    s.game_ws
        .open_files(
            &s.conn,
            &[depot_path("//game/main/Engine/logo.png")],
            OpenAction::Edit,
            LocalClId::DEFAULT,
        )
        .await
        .unwrap();
    let bob = s.server.add_user("bob").await;
    let other = TestClient::new();
    let mut ws = other.create_workspace(&s.server, "ws", &depot_path("//engine/main"));
    ws.sync(&bob, &other.cache(), None).await.unwrap();
    let err = ws
        .open_files(
            &bob,
            &[depot_path("//engine/main/Source/logo.png")],
            OpenAction::Edit,
            LocalClId::DEFAULT,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Conflict),
        "one lock for both paths: {err}"
    );
}

#[tokio::test]
async fn protections_on_the_source_apply_through_imports() {
    let s = Studio::new(true).await;
    let bob = s.server.add_user("bob").await;
    s.server
        .admin(Request::SetProtections {
            source: "write * //...\nnone user:bob //engine/...\n".into(),
        })
        .await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&s.server, "ws", &depot_path("//game/main"));
    ws.sync(&bob, &client.cache(), None).await.unwrap();
    assert!(!client.sandbox().exists("ws/Engine/a.h"));
    let err = bob
        .call(&Request::Filelog {
            path: "//game/main/Engine/a.h".into(),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );
}
