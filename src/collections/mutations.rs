//! Write operations on the collections tables. Custom collections support
//! create / rename / delete and add/remove of directory members. Directory
//! collections can only be renamed (which propagates to the directory's
//! `label`); every other mutation returns `DirectoryCollectionImmutable`.

use sqlx::{Row, SqlitePool};

use super::{
    reads::get,
    types::{internal, Collection, MutationError},
};
use crate::{
    clock::ClockRef,
    ids::{CollectionId, DirectoryId},
};

pub async fn create_custom(
    pool: &SqlitePool,
    clock: &ClockRef,
    name: &str,
    directory_ids: &[DirectoryId],
) -> Result<Collection, MutationError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(MutationError::EmptyName);
    }
    let now_s = clock.now().to_rfc3339();
    let mut tx = pool.begin().await.map_err(internal)?;

    let row = sqlx::query(
        "INSERT INTO collections (name, kind, directory_id, hidden, created_at, updated_at) \
         VALUES (?, 'custom', NULL, 0, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(&now_s)
    .bind(&now_s)
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;
    let coll_id = CollectionId(row.get::<i64, _>(0));

    for did in directory_ids {
        // Validate: directory must exist and not be soft-removed.
        let dir_row: Option<(i64,)> =
            sqlx::query_as("SELECT removed FROM directories WHERE id = ?")
                .bind(did.raw())
                .fetch_optional(&mut *tx)
                .await
                .map_err(internal)?;
        match dir_row {
            None => return Err(MutationError::DirectoryNotFound),
            Some((removed,)) if removed != 0 => return Err(MutationError::DirectoryRemoved),
            Some(_) => {}
        }
        sqlx::query(
            "INSERT OR IGNORE INTO collection_directories \
             (collection_id, directory_id, added_at) VALUES (?, ?, ?)",
        )
        .bind(coll_id.raw())
        .bind(did.raw())
        .bind(&now_s)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    }

    tx.commit().await.map_err(internal)?;
    let c = get(pool, coll_id).await.map_err(internal)?;
    c.ok_or(MutationError::NotFound)
}

pub async fn rename(
    pool: &SqlitePool,
    clock: &ClockRef,
    id: CollectionId,
    name: &str,
) -> Result<Collection, MutationError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(MutationError::EmptyName);
    }
    let now_s = clock.now().to_rfc3339();
    let mut tx = pool.begin().await.map_err(internal)?;

    let kind: Option<String> = sqlx::query("SELECT kind FROM collections WHERE id = ?")
        .bind(id.raw())
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .map(|r| r.get(0));
    let Some(kind) = kind else {
        return Err(MutationError::NotFound);
    };

    sqlx::query("UPDATE collections SET name = ?, updated_at = ? WHERE id = ?")
        .bind(name)
        .bind(&now_s)
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;

    // If directory, also update the directories.label to keep in sync.
    if kind == "directory" {
        sqlx::query(
            "UPDATE directories SET label = ? \
             WHERE id = (SELECT directory_id FROM collections WHERE id = ?)",
        )
        .bind(name)
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    }

    tx.commit().await.map_err(internal)?;
    let c = get(pool, id).await.map_err(internal)?;
    c.ok_or(MutationError::NotFound)
}

pub async fn delete_custom(pool: &SqlitePool, id: CollectionId) -> Result<(), MutationError> {
    let kind: Option<String> = sqlx::query("SELECT kind FROM collections WHERE id = ?")
        .bind(id.raw())
        .fetch_optional(pool)
        .await
        .map_err(internal)?
        .map(|r| r.get(0));
    let Some(kind) = kind else {
        return Err(MutationError::NotFound);
    };
    if kind != "custom" {
        return Err(MutationError::DirectoryCollectionImmutable);
    }
    sqlx::query("DELETE FROM collections WHERE id = ?")
        .bind(id.raw())
        .execute(pool)
        .await
        .map_err(internal)?;
    Ok(())
}

/// Add a directory to a custom collection. Rejects if the collection is of
/// kind `directory`, if the directory does not exist, or if it is currently
/// soft-removed.
pub async fn add_directory(
    pool: &SqlitePool,
    clock: &ClockRef,
    id: CollectionId,
    directory_id: DirectoryId,
) -> Result<(), MutationError> {
    let mut tx = pool.begin().await.map_err(internal)?;
    let kind: Option<String> = sqlx::query("SELECT kind FROM collections WHERE id = ?")
        .bind(id.raw())
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .map(|r| r.get(0));
    let Some(kind) = kind else {
        return Err(MutationError::NotFound);
    };
    if kind != "custom" {
        return Err(MutationError::DirectoryCollectionImmutable);
    }

    let dir_row: Option<(i64,)> = sqlx::query_as("SELECT removed FROM directories WHERE id = ?")
        .bind(directory_id.raw())
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?;
    match dir_row {
        None => return Err(MutationError::DirectoryNotFound),
        Some((removed,)) if removed != 0 => return Err(MutationError::DirectoryRemoved),
        Some(_) => {}
    }

    let now_s = clock.now().to_rfc3339();
    sqlx::query(
        "INSERT OR IGNORE INTO collection_directories \
         (collection_id, directory_id, added_at) VALUES (?, ?, ?)",
    )
    .bind(id.raw())
    .bind(directory_id.raw())
    .bind(&now_s)
    .execute(&mut *tx)
    .await
    .map_err(internal)?;

    // Bump updated_at so the collection page can rely on it for cache busts.
    sqlx::query("UPDATE collections SET updated_at = ? WHERE id = ?")
        .bind(&now_s)
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;

    tx.commit().await.map_err(internal)?;
    Ok(())
}

pub async fn remove_directory(
    pool: &SqlitePool,
    clock: &ClockRef,
    id: CollectionId,
    directory_id: DirectoryId,
) -> Result<(), MutationError> {
    let mut tx = pool.begin().await.map_err(internal)?;
    let kind: Option<String> = sqlx::query("SELECT kind FROM collections WHERE id = ?")
        .bind(id.raw())
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .map(|r| r.get(0));
    let Some(kind) = kind else {
        return Err(MutationError::NotFound);
    };
    if kind != "custom" {
        return Err(MutationError::DirectoryCollectionImmutable);
    }
    let now_s = clock.now().to_rfc3339();
    sqlx::query("DELETE FROM collection_directories WHERE collection_id = ? AND directory_id = ?")
        .bind(id.raw())
        .bind(directory_id.raw())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    sqlx::query("UPDATE collections SET updated_at = ? WHERE id = ?")
        .bind(&now_s)
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Tests for the mutation surface: create_custom, delete_custom,
    //! add_directory, and the rejections for directory-kind collections
    //! and soft-removed directories.

    use super::super::{get, test_helpers::setup, Kind, MutationError};
    use super::*;
    use crate::directories::add as add_dir;

    #[tokio::test]
    async fn create_and_delete_custom() {
        let (_tmp, pool, clock) = setup().await;
        let c = create_custom(&pool, &clock, "My Collection", &[])
            .await
            .unwrap();
        assert_eq!(c.kind, Kind::Custom);
        delete_custom(&pool, c.id).await.unwrap();
        assert!(get(&pool, c.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn directory_collection_cannot_be_deleted() {
        let (tmp, pool, clock) = setup().await;
        let videos = tmp.path().join("videos");
        std::fs::create_dir_all(&videos).unwrap();
        let dir = add_dir(&pool, &clock, &videos, Some("x".into()))
            .await
            .unwrap();
        let err = delete_custom(&pool, dir.collection_id).await.unwrap_err();
        assert!(matches!(err, MutationError::DirectoryCollectionImmutable));
    }

    #[tokio::test]
    async fn reject_add_removed_directory_to_custom() {
        let (tmp, pool, clock) = setup().await;
        let dir_a = tmp.path().join("a");
        std::fs::create_dir_all(&dir_a).unwrap();
        let a = add_dir(&pool, &clock, &dir_a, Some("A".into()))
            .await
            .unwrap();
        crate::directories::soft_remove(&pool, &clock, a.id)
            .await
            .unwrap();
        let c = create_custom(&pool, &clock, "C", &[]).await.unwrap();
        let err = add_directory(&pool, &clock, c.id, a.id).await.unwrap_err();
        assert!(matches!(err, MutationError::DirectoryRemoved));
    }

    #[tokio::test]
    async fn reject_add_directory_to_directory_collection() {
        let (tmp, pool, clock) = setup().await;
        let dir_a = tmp.path().join("a");
        std::fs::create_dir_all(&dir_a).unwrap();
        let a = add_dir(&pool, &clock, &dir_a, Some("A".into()))
            .await
            .unwrap();
        let err = add_directory(&pool, &clock, a.collection_id, a.id)
            .await
            .unwrap_err();
        assert!(matches!(err, MutationError::DirectoryCollectionImmutable));
    }
}
