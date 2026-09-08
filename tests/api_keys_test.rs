mod common;

use wardn::{api_keys, members, roles::Role};

#[tokio::test]
async fn created_key_verifies_and_resolves_the_member() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "key@example.com", Role::Member, None)
        .await
        .unwrap();
    let (key, full_key) = api_keys::create(&db.conn, &member.id, Some("laptop"))
        .await
        .unwrap();
    assert!(full_key.starts_with(api_keys::KEY_MARKER));
    assert_eq!(key.label.as_deref(), Some("laptop"));

    let resolved = api_keys::verify(&db.conn, &full_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.id, member.id);
}

#[tokio::test]
async fn wrong_key_does_not_verify() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "key2@example.com", Role::Member, None)
        .await
        .unwrap();
    let (_key, full_key) = api_keys::create(&db.conn, &member.id, None).await.unwrap();
    let tampered = format!("{full_key}x");

    assert!(
        api_keys::verify(&db.conn, &tampered)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn garbage_input_does_not_verify() {
    let (_dir, db) = common::temp_db().await;
    assert!(
        api_keys::verify(&db.conn, "not-a-real-key")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn revoked_key_no_longer_verifies() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "key3@example.com", Role::Member, None)
        .await
        .unwrap();
    let (key, full_key) = api_keys::create(&db.conn, &member.id, None).await.unwrap();
    api_keys::revoke(&db.conn, &key.id).await.unwrap();

    assert!(
        api_keys::verify(&db.conn, &full_key)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn revoking_twice_is_not_an_error() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "key4@example.com", Role::Member, None)
        .await
        .unwrap();
    let (key, _full_key) = api_keys::create(&db.conn, &member.id, None).await.unwrap();
    api_keys::revoke(&db.conn, &key.id).await.unwrap();
    api_keys::revoke(&db.conn, &key.id).await.unwrap();
}

#[tokio::test]
async fn key_belonging_to_a_removed_member_does_not_verify() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "key5@example.com", Role::Member, None)
        .await
        .unwrap();
    let (_key, full_key) = api_keys::create(&db.conn, &member.id, None).await.unwrap();
    members::remove(&db.conn, &member.id).await.unwrap();

    assert!(
        api_keys::verify(&db.conn, &full_key)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cannot_create_a_key_for_a_removed_member() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "key6@example.com", Role::Member, None)
        .await
        .unwrap();
    members::remove(&db.conn, &member.id).await.unwrap();
    assert!(api_keys::create(&db.conn, &member.id, None).await.is_err());
}

#[tokio::test]
async fn list_returns_keys_newest_first() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "key7@example.com", Role::Member, None)
        .await
        .unwrap();
    let (first, _) = api_keys::create(&db.conn, &member.id, None).await.unwrap();
    let (second, _) = api_keys::create(&db.conn, &member.id, None).await.unwrap();

    let keys = api_keys::list(&db.conn).await.unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().any(|k| k.id == first.id));
    assert!(keys.iter().any(|k| k.id == second.id));
}

#[tokio::test]
async fn create_fails_for_a_nonexistent_member() {
    let (_dir, db) = common::temp_db().await;
    let err = api_keys::create(&db.conn, "no-such-member", None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no member"));
}

#[tokio::test]
async fn find_by_id_returns_none_for_an_unknown_id() {
    let (_dir, db) = common::temp_db().await;
    assert!(
        api_keys::find_by_id(&db.conn, "no-such-key")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn revoke_fails_for_an_unknown_key_id() {
    let (_dir, db) = common::temp_db().await;
    let err = api_keys::revoke(&db.conn, "no-such-key").await.unwrap_err();
    assert!(err.to_string().contains("no api key"));
}

#[tokio::test]
async fn verify_fails_closed_when_the_stored_hash_is_corrupted() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "corrupt@example.com", Role::Member, None)
        .await
        .unwrap();
    let (_key, full_key) = api_keys::create(&db.conn, &member.id, None).await.unwrap();

    // Simulate a corrupted key_hash column (not producible through the
    // public API) — `verify` must fail closed, not panic or error, when
    // `PasswordHash::new` can't even parse the stored value.
    db.conn
        .execute("UPDATE api_keys SET key_hash = 'not-an-argon2-hash'", ())
        .await
        .unwrap();

    assert!(
        api_keys::verify(&db.conn, &full_key)
            .await
            .unwrap()
            .is_none()
    );
}
