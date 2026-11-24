//! Operator notices and runtime flags (admin panel). See `crate::notices`.

use anyhow::Result;

use super::{now_secs, Store};
use crate::notices::{Flags, Kind, NewNotice, Notice};

type Row = (i64, String, String, String, i64, i64, i64, i64, i64, i64, Option<String>);

/// `auto` tag of the notice kept by the automatic update checker (`crate::updates`).
pub const AUTO_UPDATE: &str = "update";

fn from_row(r: Row) -> Option<Notice> {
    Some(Notice {
        id: r.0,
        kind: Kind::parse(&r.1)?,
        title: r.2,
        message: r.3,
        starts_at: r.4,
        ends_at: r.5,
        show_from: r.6,
        active: r.7 != 0,
        created_at: r.8,
        updated_at: r.9,
        auto: r.10.is_some(),
    })
}

const COLUMNS: &str = "id, kind, title, message, starts_at, ends_at, show_from, active, created_at, updated_at, auto";

const FLAG_MAINTENANCE: &str = "maintenance_mode";
const FLAG_YTDLP_PAUSED: &str = "ytdlp_updates_paused";
/// JSON settings of the automatic update notices (`crate::updates::Settings`).
const KEY_AUTO_UPDATE: &str = "auto_update";

impl Store {
    /// Every notice, soonest first.
    pub async fn notices(&self) -> Result<Vec<Notice>> {
        let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM admin_notices ORDER BY starts_at, id"))
            .fetch_all(self.pool())
            .await?;
        Ok(rows.into_iter().filter_map(from_row).collect())
    }

    pub async fn notice_count(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM admin_notices")
            .fetch_one(self.pool())
            .await?;
        Ok(n)
    }

    pub async fn insert_notice(&self, n: &NewNotice) -> Result<i64> {
        let now = now_secs();
        let res = sqlx::query(
            "INSERT INTO admin_notices (kind, title, message, starts_at, ends_at, show_from, active, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(n.kind.as_str())
        .bind(&n.title)
        .bind(&n.message)
        .bind(n.starts_at)
        .bind(n.ends_at)
        .bind(n.show_from)
        .bind(i64::from(n.active))
        .bind(now)
        .bind(now)
        .execute(self.pool())
        .await?;
        Ok(res.last_insert_rowid())
    }

    /// False when there is no such notice.
    pub async fn update_notice(&self, id: i64, n: &NewNotice) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE admin_notices SET kind = ?, title = ?, message = ?, starts_at = ?, ends_at = ?, show_from = ?, \
             active = ?, updated_at = ? WHERE id = ?",
        )
        .bind(n.kind.as_str())
        .bind(&n.title)
        .bind(&n.message)
        .bind(n.starts_at)
        .bind(n.ends_at)
        .bind(n.show_from)
        .bind(i64::from(n.active))
        .bind(now_secs())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete_notice(&self, id: i64) -> Result<bool> {
        let res = sqlx::query("DELETE FROM admin_notices WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Drop notices that ended before `before` (epoch seconds).
    pub async fn prune_notices(&self, before: i64) -> Result<u64> {
        let res = sqlx::query("DELETE FROM admin_notices WHERE ends_at < ?")
            .bind(before)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    pub async fn admin_flags(&self) -> Result<Flags> {
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM admin_flags")
            .fetch_all(self.pool())
            .await?;
        let on = |k: &str| rows.iter().any(|(key, v)| key == k && v == "1");
        Ok(Flags {
            maintenance_mode: on(FLAG_MAINTENANCE),
            ytdlp_updates_paused: on(FLAG_YTDLP_PAUSED),
        })
    }

    pub async fn set_admin_flags(&self, f: &Flags) -> Result<()> {
        let mut tx = self.begin_write().await?;
        for (k, v) in [
            (FLAG_MAINTENANCE, f.maintenance_mode),
            (FLAG_YTDLP_PAUSED, f.ytdlp_updates_paused),
        ] {
            sqlx::query(
                "INSERT INTO admin_flags (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(k)
            .bind(if v { "1" } else { "0" })
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The notice kept by the automatic update checker, if any.
    pub async fn auto_notice(&self) -> Result<Option<Notice>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM admin_notices WHERE auto = ? ORDER BY id LIMIT 1"
        ))
        .bind(AUTO_UPDATE)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.and_then(from_row))
    }

    /// Write the automatic update notice: the existing one is updated (a stray extra one is
    /// dropped), or a new one is created. Notices made by hand are never touched. Returns its id.
    pub async fn upsert_auto_notice(&self, n: &NewNotice) -> Result<i64> {
        let now = now_secs();
        let mut tx = self.begin_write().await?;
        let ids: Vec<(i64,)> = sqlx::query_as("SELECT id FROM admin_notices WHERE auto = ? ORDER BY id")
            .bind(AUTO_UPDATE)
            .fetch_all(&mut *tx)
            .await?;
        let id = match ids.first() {
            Some(&(id,)) => {
                sqlx::query("DELETE FROM admin_notices WHERE auto = ? AND id <> ?")
                    .bind(AUTO_UPDATE)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "UPDATE admin_notices SET kind = ?, title = ?, message = ?, starts_at = ?, ends_at = ?, \
                     show_from = ?, active = ?, updated_at = ? WHERE id = ?",
                )
                .bind(n.kind.as_str())
                .bind(&n.title)
                .bind(&n.message)
                .bind(n.starts_at)
                .bind(n.ends_at)
                .bind(n.show_from)
                .bind(i64::from(n.active))
                .bind(now)
                .bind(id)
                .execute(&mut *tx)
                .await?;
                id
            }
            None => sqlx::query(
                "INSERT INTO admin_notices (kind, title, message, starts_at, ends_at, show_from, active, created_at, \
                 updated_at, auto) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(n.kind.as_str())
            .bind(&n.title)
            .bind(&n.message)
            .bind(n.starts_at)
            .bind(n.ends_at)
            .bind(n.show_from)
            .bind(i64::from(n.active))
            .bind(now)
            .bind(now)
            .bind(AUTO_UPDATE)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid(),
        };
        tx.commit().await?;
        Ok(id)
    }

    /// Remove the automatic update notice. Returns how many rows went away.
    pub async fn delete_auto_notices(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM admin_notices WHERE auto = ?")
            .bind(AUTO_UPDATE)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    /// Saved settings of the automatic update notices (raw JSON), if any.
    pub async fn auto_update_settings(&self) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM admin_flags WHERE key = ?")
            .bind(KEY_AUTO_UPDATE)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(|r| r.0))
    }

    pub async fn set_auto_update_settings(&self, json: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO admin_flags (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(KEY_AUTO_UPDATE)
        .bind(json)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Empty the SQLite lookup cache (searches, lists, YouTube matches); everything there is
    /// fetched again on demand. Returns how many entries were removed.
    pub async fn cache_clear(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM cache").execute(self.pool()).await?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(kind: Kind, starts_at: i64) -> NewNotice {
        NewNotice {
            kind,
            title: "T".into(),
            message: "M".into(),
            starts_at,
            ends_at: starts_at + 600,
            show_from: starts_at - 3600,
            active: true,
        }
    }

    #[tokio::test]
    async fn notices_crud_and_flags() {
        let store = Store::open_in_memory().await.unwrap();
        let b = store.insert_notice(&new(Kind::Info, 2_000)).await.unwrap();
        let a = store.insert_notice(&new(Kind::Maintenance, 1_000)).await.unwrap();
        let all = store.notices().await.unwrap();
        assert_eq!(
            all.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![a, b],
            "soonest first"
        );
        assert_eq!(all[0].kind, Kind::Maintenance);
        assert_eq!(all[0].ends_at, 1_600);

        let mut edit = new(Kind::Warning, 5_000);
        edit.active = false;
        assert!(store.update_notice(a, &edit).await.unwrap());
        assert!(!store.update_notice(999, &edit).await.unwrap());
        let n = store.notices().await.unwrap().into_iter().find(|n| n.id == a).unwrap();
        assert_eq!((n.kind, n.active, n.starts_at), (Kind::Warning, false, 5_000));

        assert_eq!(
            store.prune_notices(2_700).await.unwrap(),
            1,
            "the info notice ended at 2600"
        );
        assert!(store.delete_notice(a).await.unwrap());
        assert!(!store.delete_notice(a).await.unwrap());
        assert_eq!(store.notice_count().await.unwrap(), 0);

        assert_eq!(store.admin_flags().await.unwrap(), Flags::default());
        let f = Flags {
            maintenance_mode: true,
            ytdlp_updates_paused: false,
        };
        store.set_admin_flags(&f).await.unwrap();
        assert_eq!(store.admin_flags().await.unwrap(), f);
    }

    #[tokio::test]
    async fn auto_notice_is_kept_apart_from_manual_ones() {
        let store = Store::open_in_memory().await.unwrap();
        let manual = store.insert_notice(&new(Kind::Maintenance, 1_000)).await.unwrap();
        assert!(store.auto_notice().await.unwrap().is_none());
        assert_eq!(store.delete_auto_notices().await.unwrap(), 0);

        let a = store.upsert_auto_notice(&new(Kind::Maintenance, 5_000)).await.unwrap();
        assert_ne!(a, manual);
        let n = store.auto_notice().await.unwrap().unwrap();
        assert_eq!((n.id, n.starts_at, n.auto), (a, 5_000, true));
        // A second upsert moves the same notice.
        let b = store.upsert_auto_notice(&new(Kind::Maintenance, 9_000)).await.unwrap();
        assert_eq!(a, b, "one automatic notice, updated in place");
        assert_eq!(store.auto_notice().await.unwrap().unwrap().starts_at, 9_000);
        assert_eq!(store.notice_count().await.unwrap(), 2);

        let all = store.notices().await.unwrap();
        assert!(!all.iter().find(|n| n.id == manual).unwrap().auto);
        assert_eq!(store.delete_auto_notices().await.unwrap(), 1);
        let left = store.notices().await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, manual, "the manual notice stays");

        assert!(store.auto_update_settings().await.unwrap().is_none());
        store.set_auto_update_settings(r#"{"enabled":true}"#).await.unwrap();
        store.set_auto_update_settings(r#"{"enabled":false}"#).await.unwrap();
        assert_eq!(
            store.auto_update_settings().await.unwrap().as_deref(),
            Some(r#"{"enabled":false}"#)
        );
        assert_eq!(store.admin_flags().await.unwrap(), Flags::default(), "flags unaffected");
    }

    #[tokio::test]
    async fn cache_clear_empties_the_lookup_cache() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .cache_put("search", "a", &1, std::time::Duration::from_secs(60))
            .await
            .unwrap();
        store
            .cache_put("lists", "b", &2, std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(store.cache_clear().await.unwrap(), 2);
        assert!(store.cache_get::<i32>("search", "a").await.unwrap().is_none());
    }
}
