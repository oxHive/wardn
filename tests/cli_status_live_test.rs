mod common;

/// This is the only test in this crate that sets `WARDN_LISTEN_ADDR` — env
/// vars are process-global, so exercising both of `cmd_status`'s "serve is
/// reachable" branches has to happen sequentially in one test function
/// rather than as separate `#[tokio::test]`s that could interleave.
#[tokio::test]
async fn status_reports_the_running_branches() {
    let (_dir, db_path) = common::temp_db_path();
    wardn::cli::cmd_init(&db_path, Some("Acme".to_string()))
        .await
        .unwrap();

    // A real `wardn serve` instance: hits `cmd_status`'s success-and-valid-
    // JSON branch (reports an uptime).
    let (_serve_dir, serve_db) = common::temp_db().await;
    wardn::org::create(&serve_db.conn, "Acme").await.unwrap();
    let state = wardn::serve::AppState::new(serve_db.conn, wardn::db::now());
    let real_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let real_addr = real_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(real_listener, wardn::app(state)).await.unwrap();
    });

    // A server that responds 200 but with a non-JSON body at the same
    // path: hits `cmd_status`'s success-but-unparseable-body branch.
    let fake_app =
        axum::Router::new().route("/v1/status", axum::routing::get(|| async { "not json" }));
    let fake_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fake_addr = fake_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(fake_listener, fake_app).await.unwrap();
    });

    tokio::task::yield_now().await;

    unsafe {
        std::env::set_var("WARDN_LISTEN_ADDR", real_addr.to_string());
    }
    wardn::cli::cmd_status(&db_path).await.unwrap();

    unsafe {
        std::env::set_var("WARDN_LISTEN_ADDR", fake_addr.to_string());
    }
    wardn::cli::cmd_status(&db_path).await.unwrap();

    unsafe {
        std::env::remove_var("WARDN_LISTEN_ADDR");
    }
}
