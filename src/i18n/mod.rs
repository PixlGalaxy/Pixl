//! Bot-side translations. The bot always speaks `BOT_LOCALE`; the panel has its own per-user language.

use std::collections::HashMap;

use anyhow::{Context, Result};

const EN: &str = include_str!("../../locales/en.json");
const ES: &str = include_str!("../../locales/es.json");

pub struct I18n {
    strings: HashMap<String, String>,
    fallback: HashMap<String, String>,
}

impl I18n {
    pub fn load(locale: &str) -> Result<Self> {
        let fallback = flatten(EN).context("locales/en.json")?;
        let strings = match locale {
            "es" => flatten(ES).context("locales/es.json")?,
            _ => fallback.clone(),
        };
        Ok(Self { strings, fallback })
    }

    /// Translate `key`, replacing `{name}` placeholders. Missing keys fall back to English, then to the key.
    pub fn t(&self, key: &str, args: &[(&str, &str)]) -> String {
        let template = self.strings.get(key).or_else(|| self.fallback.get(key)).map(String::as_str).unwrap_or(key);
        args.iter().fold(template.to_owned(), |acc, (name, value)| acc.replace(&format!("{{{name}}}"), value))
    }
}

/// Turn nested JSON into dotted keys: {"player": {"paused": "x"}} -> "player.paused".
fn flatten(raw: &str) -> Result<HashMap<String, String>> {
    fn walk(prefix: &str, value: &serde_json::Value, out: &mut HashMap<String, String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    walk(&key, v, out);
                }
            }
            serde_json::Value::String(s) => {
                out.insert(prefix.to_owned(), s.clone());
            }
            _ => {}
        }
    }
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let mut out = HashMap::new();
    walk("", &value, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_and_fallback() {
        let i18n = I18n::load("es").unwrap();
        assert!(!i18n.t("common.error", &[]).is_empty());
        assert_eq!(i18n.t("does.not.exist", &[]), "does.not.exist");
    }
}
