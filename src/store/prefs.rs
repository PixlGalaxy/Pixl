//! Per-user preferences: panel locale and history opt-out.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{double_option, now_secs, sf, Store};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPrefs {
    pub locale: Option<String>,
    pub history_enabled: bool,
}

impl Default for UserPrefs {
    fn default() -> Self {
        Self { locale: None, history_enabled: true }
    }
}

/// Body of `PUT /api/me/prefs`. `locale: null` clears the locale; an absent field keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPrefsPatch {
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub locale: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_enabled: Option<bool>,
}

impl Store {
    pub async fn user_prefs(&self, user_id: u64) -> Result<UserPrefs> {
        let row: Option<(Option<String>, bool)> =
            sqlx::query_as("SELECT locale, history_enabled FROM user_prefs WHERE user_id = ?")
                .bind(sf(user_id))
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|(locale, history_enabled)| UserPrefs { locale, history_enabled }).unwrap_or_default())
    }

    /// True unless the user opted out of history.
    pub async fn history_enabled(&self, user_id: u64) -> Result<bool> {
        let row: Option<(bool,)> = sqlx::query_as("SELECT history_enabled FROM user_prefs WHERE user_id = ?")
            .bind(sf(user_id))
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(|r| r.0).unwrap_or(true))
    }

    /// Apply a partial update in one statement and return the result.
    pub async fn update_user_prefs(&self, user_id: u64, patch: &UserPrefsPatch) -> Result<UserPrefs> {
        let (set_locale, locale) = match &patch.locale {
            Some(l) => (true, l.clone()),
            None => (false, None),
        };
        let (locale, history_enabled): (Option<String>, bool) = sqlx::query_as(
            "INSERT INTO user_prefs (user_id, locale, history_enabled, updated_at) \
             VALUES (?1, CASE WHEN ?2 THEN ?3 ELSE NULL END, COALESCE(?4, 1), ?5) \
             ON CONFLICT(user_id) DO UPDATE SET \
               locale = CASE WHEN ?2 THEN ?3 ELSE locale END, \
               history_enabled = COALESCE(?4, history_enabled), \
               updated_at = ?5 \
             RETURNING locale, history_enabled",
        )
        .bind(sf(user_id))
        .bind(set_locale)
        .bind(locale)
        .bind(patch.history_enabled)
        .bind(now_secs())
        .fetch_one(self.pool())
        .await?;
        Ok(UserPrefs { locale, history_enabled })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn defaults_and_partial_updates() {
        let store = Store::open_in_memory().await.unwrap();
        assert_eq!(store.user_prefs(1).await.unwrap(), UserPrefs::default());
        assert!(store.history_enabled(1).await.unwrap());

        let p = store
            .update_user_prefs(1, &UserPrefsPatch { locale: Some(Some("es".into())), history_enabled: None })
            .await
            .unwrap();
        assert_eq!(p, UserPrefs { locale: Some("es".into()), history_enabled: true });

        let p =
            store.update_user_prefs(1, &UserPrefsPatch { locale: None, history_enabled: Some(false) }).await.unwrap();
        assert_eq!(p, UserPrefs { locale: Some("es".into()), history_enabled: false });
        assert!(!store.history_enabled(1).await.unwrap());

        let patch: UserPrefsPatch = serde_json::from_str(r#"{"locale":null}"#).unwrap();
        assert_eq!(patch.locale, Some(None));
        let p = store.update_user_prefs(1, &patch).await.unwrap();
        assert_eq!(p, UserPrefs { locale: None, history_enabled: false });

        let patch: UserPrefsPatch = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(patch.locale, None);
    }
}
