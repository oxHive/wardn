mod common;

use wardn::cli::{self, KeysCommand, MembersCommand, OrgCommand, RoleCommand};

#[tokio::test]
async fn init_creates_the_org_and_is_idempotent() {
    let (_dir, db_path) = common::temp_db_path();

    cli::cmd_init(&db_path, Some("Acme".to_string()))
        .await
        .unwrap();
    // Running init again against an already-initialized database is a
    // no-op, not an error — and must not overwrite the existing org.
    cli::cmd_init(&db_path, Some("Ignored".to_string()))
        .await
        .unwrap();

    let db = wardn::Db::open(&db_path).await.unwrap();
    let org = wardn::org::get(&db.conn).await.unwrap().unwrap();
    assert_eq!(org.name, "Acme");
}

#[tokio::test]
async fn init_rejects_a_blank_name() {
    let (_dir, db_path) = common::temp_db_path();
    let err = cli::cmd_init(&db_path, Some("   ".to_string()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("empty"));
}

#[tokio::test]
async fn org_command_lifecycle() {
    let (_dir, db_path) = common::temp_db_path();
    cli::cmd_init(&db_path, Some("Acme".to_string()))
        .await
        .unwrap();

    cli::cmd_org(
        &db_path,
        OrgCommand::Rename {
            name: "Acme Renamed".to_string(),
        },
    )
    .await
    .unwrap();

    cli::cmd_org(
        &db_path,
        OrgCommand::Invite {
            email: "alice@example.com".to_string(),
            role: "admin".to_string(),
        },
    )
    .await
    .unwrap();

    cli::cmd_org(
        &db_path,
        OrgCommand::Members {
            command: MembersCommand::List,
        },
    )
    .await
    .unwrap();

    let db = wardn::Db::open(&db_path).await.unwrap();
    let org = wardn::org::get(&db.conn).await.unwrap().unwrap();
    assert_eq!(org.name, "Acme Renamed");
    let member = wardn::members::list(&db.conn)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(member.email, "alice@example.com");

    cli::cmd_org(
        &db_path,
        OrgCommand::Role {
            command: RoleCommand::Set {
                member_id: member.id.clone(),
                role: "member".to_string(),
            },
        },
    )
    .await
    .unwrap();
    let updated = wardn::members::find_by_id(&db.conn, &member.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.role, wardn::roles::Role::Member);

    cli::cmd_org(
        &db_path,
        OrgCommand::Members {
            command: MembersCommand::Remove {
                member_id: member.id.clone(),
            },
        },
    )
    .await
    .unwrap();
    assert!(wardn::members::list(&db.conn).await.unwrap().is_empty());

    // `yes: true` skips the interactive confirmation prompt — required in
    // a test, where there's no terminal to read a confirmation from.
    cli::cmd_org(&db_path, OrgCommand::Delete { yes: true })
        .await
        .unwrap();
    assert!(wardn::org::get(&db.conn).await.unwrap().is_none());
}

#[tokio::test]
async fn org_command_rejects_an_invalid_role() {
    let (_dir, db_path) = common::temp_db_path();
    cli::cmd_init(&db_path, Some("Acme".to_string()))
        .await
        .unwrap();

    let err = cli::cmd_org(
        &db_path,
        OrgCommand::Invite {
            email: "bob@example.com".to_string(),
            role: "owner".to_string(),
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("owner"));
}

#[tokio::test]
async fn keys_command_lifecycle() {
    let (_dir, db_path) = common::temp_db_path();
    cli::cmd_init(&db_path, Some("Acme".to_string()))
        .await
        .unwrap();
    cli::cmd_org(
        &db_path,
        OrgCommand::Invite {
            email: "carol@example.com".to_string(),
            role: "member".to_string(),
        },
    )
    .await
    .unwrap();

    let db = wardn::Db::open(&db_path).await.unwrap();
    let member = wardn::members::list(&db.conn)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    cli::cmd_keys(
        &db_path,
        KeysCommand::Create {
            member: member.id.clone(),
            name: Some("laptop".to_string()),
        },
    )
    .await
    .unwrap();

    let keys = wardn::api_keys::list(&db.conn).await.unwrap();
    assert_eq!(keys.len(), 1);
    let key = &keys[0];
    assert_eq!(key.label.as_deref(), Some("laptop"));

    cli::cmd_keys(&db_path, KeysCommand::List).await.unwrap();

    cli::cmd_keys(
        &db_path,
        KeysCommand::Revoke {
            key_id: key.id.clone(),
        },
    )
    .await
    .unwrap();
    let revoked = wardn::api_keys::find_by_id(&db.conn, &key.id)
        .await
        .unwrap()
        .unwrap();
    assert!(revoked.revoked_at.is_some());
}

#[tokio::test]
async fn status_reports_uninitialized_and_initialized_states() {
    let (_dir, db_path) = common::temp_db_path();

    // `cmd_status` opens the database itself (same as `Db::open` does for
    // every other command) — calling it before `init` still succeeds,
    // since `Db::open` creates the schema on first use; there's just no
    // org row yet.
    cli::cmd_status(&db_path).await.unwrap();

    cli::cmd_init(&db_path, Some("Acme".to_string()))
        .await
        .unwrap();
    cli::cmd_status(&db_path).await.unwrap();
}
