//! Per-user preferences: panel locale, history opt-out and panel appearance (theme, accent).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{double_option, now_secs, sf, Store};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPrefs {
    pub locale: Option<String>,
    pub history_enabled: bool,
    /// `light`, `dark` or `system`; None = panel default.
    pub theme: Option<String>,
    /// Accent palette id; None = panel default.
    pub accent: Option<String>,
}

impl Default for UserPrefs {
    fn default() -> Self {
        Self {
            locale: None,
            history_enabled: true,
            theme: None,
            accent: None,
        }
    }
}

/// Body of `PUT /api/me/prefs`. `null` clears a field (locale, theme, accent); an absent field keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPrefsPatch {
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub locale: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_enabled: Option<bool>,
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub theme: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub accent: Option<Option<String>>,
}

type PrefsRow = (Option<String>, bool, Option<String>, Option<String>);

fn from_row((locale, history_enabled, theme, accent): PrefsRow) -> UserPrefs {
    UserPrefs {
        locale,
        history_enabled,
        theme,
        accent,
    }
}

/// `(set, value)` binding pair of a nullable patch field.
fn split(field: &Option<Option<String>>) -> (bool, Option<String>) {
    match field {
        Some(v) => (true, v.clone()),
        None => (false, None),
    }
}

impl Store {
    pub async fn user_prefs(&self, user_id: u64) -> Result<UserPrefs> {
        let row: Option<PrefsRow> =
            sqlx::query_as("SELECT locale, history_enabled, theme, accent FROM user_prefs WHERE user_id = ?")
                .bind(sf(user_id))
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(from_row).unwrap_or_default())
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
        let (set_locale, locale) = split(&patch.locale);
        let (set_theme, theme) = split(&patch.theme);
        let (set_accent, accent) = split(&patch.accent);
        let row: PrefsRow = sqlx::query_as(
            "INSERT INTO user_prefs (user_id, locale, history_enabled, theme, accent, updated_at) \
             VALUES (?1, CASE WHEN ?2 THEN ?3 ELSE NULL END, COALESCE(?4, 1), \
                     CASE WHEN ?6 THEN ?7 ELSE NULL END, CASE WHEN ?8 THEN ?9 ELSE NULL END, ?5) \
             ON CONFLICT(user_id) DO UPDATE SET \
               locale = CASE WHEN ?2 THEN ?3 ELSE locale END, \
               history_enabled = COALESCE(?4, history_enabled), \
               theme = CASE WHEN ?6 THEN ?7 ELSE theme END, \
               accent = CASE WHEN ?8 THEN ?9 ELSE accent END, \
               updated_at = ?5 \
             RETURNING locale, history_enabled, theme, accent",
        )
        .bind(sf(user_id))
        .bind(set_locale)
        .bind(locale)
        .bind(patch.history_enabled)
        .bind(now_secs())
        .bind(set_theme)
        .bind(theme)
        .bind(set_accent)
        .bind(accent)
        .fetch_one(self.pool())
        .await?;
        Ok(from_row(row))
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
            .update_user_prefs(
                1,
                &UserPrefsPatch {
                    locale: Some(Some("es".into())),
                    history_enabled: None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            p,
            UserPrefs {
                locale: Some("es".into()),
                ..Default::default()
            }
        );

        let p = store
            .update_user_prefs(
                1,
                &UserPrefsPatch {
                    locale: None,
                    history_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            p,
            UserPrefs {
                locale: Some("es".into()),
                history_enabled: false,
                ..Default::default()
            }
        );
        assert!(!store.history_enabled(1).await.unwrap());

        let patch: UserPrefsPatch = serde_json::from_str(r#"{"locale":null}"#).unwrap();
        assert_eq!(patch.locale, Some(None));
        let p = store.update_user_prefs(1, &patch).await.unwrap();
        assert_eq!(
            p,
            UserPrefs {
                history_enabled: false,
                ..Default::default()
            }
        );

        let patch: UserPrefsPatch = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(patch.locale, None);

        // Appearance: set, keep on other updates, clear with null.
        let patch: UserPrefsPatch = serde_json::from_str(r#"{"theme":"light","accent":"forest"}"#).unwrap();
        let p = store.update_user_prefs(1, &patch).await.unwrap();
        assert_eq!(
            (p.theme.as_deref(), p.accent.as_deref()),
            (Some("light"), Some("forest"))
        );
        let p = store
            .update_user_prefs(
                1,
                &UserPrefsPatch {
                    history_enabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            (p.theme.as_deref(), p.accent.as_deref()),
            (Some("light"), Some("forest"))
        );
        let patch: UserPrefsPatch = serde_json::from_str(r#"{"accent":null}"#).unwrap();
        let p = store.update_user_prefs(1, &patch).await.unwrap();
        assert_eq!((p.theme.as_deref(), p.accent.as_deref()), (Some("light"), None));
        assert_eq!(store.user_prefs(1).await.unwrap(), p);
    }
}
