//! Operator notices and runtime flags (admin panel). See `crate::notices`.

use anyhow::Result;

use super::{now_secs, Store};
use crate::notices::{Flags, Kind, NewNotice, Notice};

type Row = (i64, String, String, String, i64, i64, i64, i64, i64, i64);

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
    })
}

const COLUMNS: &str = "id, kind, title, message, starts_at, ends_at, show_from, active, created_at, updated_at";

const FLAG_MAINTENANCE: &str = "maintenance_mode";
const FLAG_YTDLP_PAUSED: &str = "ytdlp_updates_paused";

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
