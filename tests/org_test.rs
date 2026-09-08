mod common;

use wardn::org;

#[tokio::test]
async fn no_org_until_created() {
    let (_dir, db) = common::temp_db().await;
    assert!(org::get(&db.conn).await.unwrap().is_none());
}

#[tokio::test]
async fn create_then_get_round_trips() {
    let (_dir, db) = common::temp_db().await;
    let created = org::create(&db.conn, "Acme Corp").await.unwrap();
    let fetched = org::get(&db.conn).await.unwrap().unwrap();
    assert_eq!(created.id, fetched.id);
    assert_eq!(fetched.name, "Acme Corp");
}

#[tokio::test]
async fn cannot_create_a_second_org() {
    let (_dir, db) = common::temp_db().await;
    org::create(&db.conn, "First").await.unwrap();
    let err = org::create(&db.conn, "Second").await.unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

#[tokio::test]
async fn rename_updates_name_but_keeps_id() {
    let (_dir, db) = common::temp_db().await;
    let created = org::create(&db.conn, "Old Name").await.unwrap();
    let renamed = org::rename(&db.conn, "New Name").await.unwrap();
    assert_eq!(renamed.id, created.id);
    assert_eq!(renamed.name, "New Name");
}

#[tokio::test]
async fn rename_without_an_org_fails() {
    let (_dir, db) = common::temp_db().await;
    assert!(org::rename(&db.conn, "New Name").await.is_err());
}

#[tokio::test]
async fn delete_removes_org_and_members_and_keys() {
    let (_dir, db) = common::temp_db().await;
    org::create(&db.conn, "Acme").await.unwrap();
    let member = wardn::members::invite(&db.conn, "a@example.com", wardn::roles::Role::Admin, None)
        .await
        .unwrap();
    wardn::api_keys::create(&db.conn, &member.id, None)
        .await
        .unwrap();

    org::delete(&db.conn).await.unwrap();

    assert!(org::get(&db.conn).await.unwrap().is_none());
    assert!(wardn::members::list(&db.conn).await.unwrap().is_empty());
    assert!(wardn::api_keys::list(&db.conn).await.unwrap().is_empty());
}
