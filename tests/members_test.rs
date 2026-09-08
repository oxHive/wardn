mod common;

use wardn::members;
use wardn::roles::Role;

#[tokio::test]
async fn invite_then_list() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "person@example.com", Role::Member, None)
        .await
        .unwrap();
    assert_eq!(member.email, "person@example.com");
    assert_eq!(member.role, Role::Member);

    let all = members::list(&db.conn).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, member.id);
}

#[tokio::test]
async fn email_is_normalized_to_lowercase() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, " Person@Example.COM ", Role::Member, None)
        .await
        .unwrap();
    assert_eq!(member.email, "person@example.com");
}

#[tokio::test]
async fn cannot_invite_the_same_active_email_twice() {
    let (_dir, db) = common::temp_db().await;
    members::invite(&db.conn, "dup@example.com", Role::Member, None)
        .await
        .unwrap();
    let err = members::invite(&db.conn, "dup@example.com", Role::Member, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already a member"));
}

#[tokio::test]
async fn removing_a_member_is_a_soft_delete_and_hides_them_from_list() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "gone@example.com", Role::Member, None)
        .await
        .unwrap();
    members::remove(&db.conn, &member.id).await.unwrap();

    assert!(members::list(&db.conn).await.unwrap().is_empty());
    let fetched = members::find_by_id(&db.conn, &member.id)
        .await
        .unwrap()
        .unwrap();
    assert!(fetched.removed_at.is_some());
}

#[tokio::test]
async fn removing_a_member_revokes_their_api_keys() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "keyed@example.com", Role::Member, None)
        .await
        .unwrap();
    let (key, _full_key) = wardn::api_keys::create(&db.conn, &member.id, None)
        .await
        .unwrap();

    members::remove(&db.conn, &member.id).await.unwrap();

    let key = wardn::api_keys::find_by_id(&db.conn, &key.id)
        .await
        .unwrap()
        .unwrap();
    assert!(key.revoked_at.is_some());
}

#[tokio::test]
async fn removing_twice_fails() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "once@example.com", Role::Member, None)
        .await
        .unwrap();
    members::remove(&db.conn, &member.id).await.unwrap();
    assert!(members::remove(&db.conn, &member.id).await.is_err());
}

#[tokio::test]
async fn set_role_changes_role() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "promote@example.com", Role::ReadOnly, None)
        .await
        .unwrap();
    let updated = members::set_role(&db.conn, &member.id, Role::Admin)
        .await
        .unwrap();
    assert_eq!(updated.role, Role::Admin);
}

#[tokio::test]
async fn set_role_on_removed_member_fails() {
    let (_dir, db) = common::temp_db().await;
    let member = members::invite(&db.conn, "removed@example.com", Role::Member, None)
        .await
        .unwrap();
    members::remove(&db.conn, &member.id).await.unwrap();
    assert!(
        members::set_role(&db.conn, &member.id, Role::Admin)
            .await
            .is_err()
    );
}
