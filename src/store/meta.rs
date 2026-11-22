//! Internal key/value bookkeeping (e.g. the hash of the registered slash commands).

use anyhow::Result;

use super::Store;

impl Store {
    pub async fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM meta WHERE key = ?")
            .bind(key)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(|r| r.0))
    }

    pub async fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_set() {
        let store = Store::open_in_memory().await.unwrap();
        assert!(store.meta_get("k").await.unwrap().is_none());
        store.meta_set("k", "a").await.unwrap();
        store.meta_set("k", "b").await.unwrap();
        assert_eq!(store.meta_get("k").await.unwrap().as_deref(), Some("b"));
    }
}
