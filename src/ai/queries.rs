//! Read-only SQL the AI engine needs on top of the store API (batched, bounded).

use std::collections::HashMap;

use anyhow::Result;
use sqlx::{QueryBuilder, Row, Sqlite};

use crate::store::{catalog_from_sqlite_row, CatalogTrack, Store, TrackId, CATALOG_COLUMNS, CATALOG_COLUMN_COUNT};

const CHUNK: usize = 400;

/// Tags of many tracks at once.
pub async fn tags_for_tracks(store: &Store, ids: &[TrackId]) -> Result<HashMap<TrackId, Vec<(String, f64)>>> {
    let mut out: HashMap<TrackId, Vec<(String, f64)>> = HashMap::new();
    for chunk in ids.chunks(CHUNK) {
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT track_id, tag, weight FROM track_tags WHERE track_id IN (");
        let mut sep = qb.separated(", ");
        for id in chunk {
            sep.push_bind(*id);
        }
        qb.push(") ORDER BY track_id, weight DESC");
        let rows: Vec<(i64, String, f64)> = qb.build_query_as().fetch_all(store.pool()).await?;
        for (id, tag, w) in rows {
            out.entry(id).or_default().push((tag, w));
        }
    }
    Ok(out)
}

/// Catalog rows by id (order not preserved).
pub async fn catalog_tracks(store: &Store, ids: &[TrackId]) -> Result<Vec<CatalogTrack>> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(CHUNK) {
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new(format!("SELECT {CATALOG_COLUMNS} FROM tracks t WHERE t.id IN ("));
        let mut sep = qb.separated(", ");
        for id in chunk {
            sep.push_bind(*id);
        }
        qb.push(")");
        for r in qb.build().fetch_all(store.pool()).await? {
            out.push(catalog_from_sqlite_row(&r)?);
        }
    }
    Ok(out)
}

/// Most played tracks (by users with history on) tagged with any of `genres`, scored by
/// tag weight * total plays. With no genres: globally most played.
pub async fn popular_in_genres(store: &Store, genres: &[&str], limit: u32) -> Result<Vec<(CatalogTrack, f64)>> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("");
    if genres.is_empty() {
        qb.push(format!(
            "SELECT {CATALOG_COLUMNS}, CAST(SUM(h.play_count) AS REAL) AS s FROM user_history h \
             JOIN tracks t ON t.id = h.track_id LEFT JOIN user_prefs p ON p.user_id = h.user_id \
             WHERE COALESCE(p.history_enabled, 1) = 1 GROUP BY t.id ORDER BY s DESC LIMIT "
        ));
        qb.push_bind(limit);
    } else {
        qb.push("WITH g AS (SELECT track_id, MAX(weight) AS w FROM track_tags WHERE tag IN (");
        let mut sep = qb.separated(", ");
        for g in genres {
            sep.push_bind(g.to_string());
        }
        qb.push(format!(
            ") GROUP BY track_id ORDER BY w DESC LIMIT 5000) \
             SELECT {CATALOG_COLUMNS}, g.w * SUM(h.play_count) AS s FROM g \
             JOIN tracks t ON t.id = g.track_id JOIN user_history h ON h.track_id = g.track_id \
             LEFT JOIN user_prefs p ON p.user_id = h.user_id \
             WHERE COALESCE(p.history_enabled, 1) = 1 GROUP BY t.id ORDER BY s DESC LIMIT "
        ));
        qb.push_bind(limit);
    }
    let rows = qb.build().fetch_all(store.pool()).await?;
    rows.iter()
        .map(|r| Ok((catalog_from_sqlite_row(r)?, r.try_get::<f64, _>(CATALOG_COLUMN_COUNT)?)))
        .collect()
}

/// Number of distinct tracks `user` shares with each of `others`.
pub async fn shared_track_counts(store: &Store, user: u64, others: &[u64]) -> Result<HashMap<u64, u32>> {
    let mut out = HashMap::new();
    for chunk in others.chunks(CHUNK) {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT b.user_id, COUNT(*) FROM user_history a JOIN user_history b ON b.track_id = a.track_id \
             WHERE a.user_id = ",
        );
        qb.push_bind(user as i64).push(" AND b.user_id IN (");
        let mut sep = qb.separated(", ");
        for id in chunk {
            sep.push_bind(*id as i64);
        }
        qb.push(") GROUP BY b.user_id");
        let rows: Vec<(i64, u32)> = qb.build_query_as().fetch_all(store.pool()).await?;
        out.extend(rows.into_iter().map(|(u, n)| (u as u64, n)));
    }
    Ok(out)
}
