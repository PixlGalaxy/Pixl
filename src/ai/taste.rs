//! Taste vectors (genre -> weight) and similar users, recomputed in the background.
//!
//! Vector: sum over history rows of `tag_weight * (1 + ln(plays)) * 0.5^(age / half_life)`,
//! plus `tag_weight * FEEDBACK_WEIGHT * feedback` (likes add, dislikes subtract), L2-normalized.
//! Neighbors: cosine over vectors plus a small bonus for shared tracks, top K kept.

use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::queries;
use crate::store::{now_secs, Store, TasteInput, TasteVector, UserTaste};

pub const HALF_LIFE_DAYS: f64 = 120.0;
pub const FEEDBACK_WEIGHT: f64 = 1.5;
/// Genres kept per vector.
pub const MAX_GENRES: usize = 40;
/// Neighbors kept per user.
pub const NEIGHBORS_K: usize = 30;
/// Cosine shortlist size before the shared-track bonus is applied.
const SHORTLIST: usize = 60;
const MIN_NEIGHBOR_SCORE: f64 = 0.15;
/// Shared tracks at which the bonus saturates, and its maximum.
const SHARED_SATURATION: f64 = 50.0;
const SHARED_BONUS: f64 = 0.25;
/// Users recomputed per round; vectors paged per similarity scan.
const STALE_BATCH: u32 = 200;
const PAGE: u32 = 1000;

/// Compute one user's vector from their history/feedback rows.
pub fn compute_vector(inputs: &[TasteInput], now: i64) -> TasteVector {
    let mut acc: HashMap<String, f64> = HashMap::new();
    for i in inputs {
        let age_days = ((now - i.last_played).max(0) as f64) / 86_400.0;
        let recency = 0.5f64.powf(age_days / HALF_LIFE_DAYS);
        let plays = if i.play_count > 0 {
            (1.0 + f64::from(i.play_count).ln()) * recency
        } else {
            0.0
        };
        let fb = FEEDBACK_WEIGHT * i.feedback.signum() as f64;
        *acc.entry(i.tag.clone()).or_insert(0.0) += i.tag_weight.clamp(0.0, 1.0) * (plays + fb);
    }
    normalize(acc, MAX_GENRES)
}

/// Keep the `max` strongest entries (by magnitude) and scale to unit L2 length.
pub fn normalize(v: HashMap<String, f64>, max: usize) -> TasteVector {
    let mut items: Vec<(String, f64)> = v.into_iter().filter(|(_, w)| w.is_finite() && w.abs() > 1e-9).collect();
    items.sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()).then_with(|| a.0.cmp(&b.0)));
    items.truncate(max);
    let norm = items.iter().map(|(_, w)| w * w).sum::<f64>().sqrt();
    if norm <= 0.0 {
        return TasteVector::new();
    }
    items
        .into_iter()
        .map(|(g, w)| (g, ((w / norm) * 10_000.0).round() / 10_000.0))
        .filter(|(_, w)| w.abs() >= 0.005)
        .collect()
}

pub fn cosine(a: &TasteVector, b: &TasteVector) -> f64 {
    let (small, big) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let dot: f64 = small.iter().filter_map(|(k, x)| big.get(k).map(|y| x * y)).sum();
    let na = a.values().map(|x| x * x).sum::<f64>().sqrt();
    let nb = b.values().map(|x| x * x).sum::<f64>().sqrt();
    if na <= 0.0 || nb <= 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Final neighbor score: cosine plus a saturating bonus for tracks in common.
pub fn neighbor_score(cos: f64, shared_tracks: u32) -> f64 {
    cos + SHARED_BONUS * (f64::from(shared_tracks) / SHARED_SATURATION).min(1.0)
}

/// Weighted mix of several vectors (group mode, prompt genres). Each input is normalized first so
/// a heavy listener does not drown the others.
pub fn blend(parts: &[(f64, &TasteVector)]) -> TasteVector {
    let mut acc: HashMap<String, f64> = HashMap::new();
    for (w, v) in parts {
        let n = v.values().map(|x| x * x).sum::<f64>().sqrt();
        if n <= 0.0 || *w <= 0.0 {
            continue;
        }
        for (g, x) in v.iter() {
            *acc.entry(g.clone()).or_insert(0.0) += w * x / n;
        }
    }
    normalize(acc, MAX_GENRES)
}

/// Recompute up to `STALE_BATCH` stale vectors; returns the users that changed.
pub async fn recompute_stale(store: &Store, cancel: &CancellationToken) -> Result<Vec<(u64, TasteVector)>> {
    let stale = store.stale_tastes(STALE_BATCH).await?;
    let now = now_secs();
    let mut done = Vec::with_capacity(stale.len());
    for s in stale {
        if cancel.is_cancelled() {
            break;
        }
        let inputs = store.taste_inputs(s.user_id).await?;
        let v = compute_vector(&inputs, now);
        store.put_taste(s.user_id, &v, s.version).await?;
        if store.history_enabled(s.user_id).await? {
            done.push((s.user_id, v));
        }
    }
    Ok(done)
}

/// Recompute the neighbor lists of `users` with one paged scan over every vector.
pub async fn update_neighbors(store: &Store, users: &[(u64, TasteVector)], cancel: &CancellationToken) -> Result<()> {
    if users.is_empty() {
        return Ok(());
    }
    // Per user: shortlist of (other, cosine), kept bounded.
    let mut short: Vec<Vec<(u64, f64)>> = vec![Vec::new(); users.len()];
    let mut after = 0u64;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let page: Vec<UserTaste> = store.taste_page(after, PAGE).await?;
        let Some(last) = page.last() else { break };
        after = last.user_id;
        for (i, (uid, v)) in users.iter().enumerate() {
            for other in &page {
                if other.user_id == *uid {
                    continue;
                }
                let c = cosine(v, &other.vector);
                if c > MIN_NEIGHBOR_SCORE / 2.0 {
                    short[i].push((other.user_id, c));
                }
            }
            if short[i].len() > SHORTLIST * 4 {
                trim(&mut short[i], SHORTLIST);
            }
        }
        if page.len() < PAGE as usize {
            break;
        }
        tokio::task::yield_now().await;
    }
    for (i, (uid, _)) in users.iter().enumerate() {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let list = &mut short[i];
        trim(list, SHORTLIST);
        let others: Vec<u64> = list.iter().map(|(u, _)| *u).collect();
        let shared = queries::shared_track_counts(store, *uid, &others).await?;
        let mut scored: Vec<(u64, f64)> = list
            .iter()
            .map(|(u, c)| (*u, neighbor_score(*c, shared.get(u).copied().unwrap_or(0))))
            .filter(|(_, s)| *s >= MIN_NEIGHBOR_SCORE)
            .collect();
        trim(&mut scored, NEIGHBORS_K);
        store.replace_neighbors(*uid, &scored).await?;
    }
    Ok(())
}

fn trim(v: &mut Vec<(u64, f64)>, n: usize) {
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    v.truncate(n);
}

/// Background loop: every `interval`, recompute stale vectors and their neighbors until idle.
pub async fn run(store: Store, interval: Duration, stop: CancellationToken) {
    loop {
        loop {
            match recompute_stale(&store, &stop).await {
                Ok(users) if users.is_empty() => break,
                Ok(users) => {
                    debug!(users = users.len(), "taste vectors recomputed");
                    if let Err(e) = update_neighbors(&store, &users, &stop).await {
                        warn!(error = %e, "neighbor update failed");
                        break;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "taste recompute failed");
                    break;
                }
            }
            if stop.is_cancelled() {
                return;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = stop.cancelled() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{test_util::track, Feedback, Tag, TagSource, UserPrefsPatch};

    fn input(tag: &str, plays: u32, age_days: i64, feedback: i64) -> TasteInput {
        TasteInput {
            tag: tag.into(),
            tag_weight: 1.0,
            play_count: plays,
            last_played: 1_000_000_000 - age_days * 86_400,
            feedback,
        }
    }

    fn v(pairs: &[(&str, f64)]) -> TasteVector {
        pairs.iter().map(|(g, w)| (g.to_string(), *w)).collect()
    }

    #[test]
    fn vector_math() {
        let now = 1_000_000_000;
        let out = compute_vector(&[input("rock", 10, 0, 0), input("jazz", 10, 240, 0)], now);
        // Same plays, jazz is two half-lives old: a quarter of the weight.
        assert!((out["rock"] / out["jazz"] - 4.0).abs() < 0.01, "{out:?}");
        let len: f64 = out.values().map(|x| x * x).sum::<f64>().sqrt();
        assert!((len - 1.0).abs() < 1e-3);

        // Sublinear plays: 100 plays is not 100x one play.
        let out = compute_vector(&[input("a", 100, 0, 0), input("b", 1, 0, 0)], now);
        assert!(out["a"] / out["b"] < 6.0);

        // Likes add, dislikes subtract (can go negative).
        let out = compute_vector(&[input("pop", 1, 0, -1), input("salsa", 0, 0, 1)], now);
        assert!(out["pop"] < 0.0 && out["salsa"] > 0.0, "{out:?}");
        assert!(compute_vector(&[], now).is_empty());
    }

    #[test]
    fn similarity() {
        let a = v(&[("rock", 0.8), ("indie", 0.6)]);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-9);
        let b = v(&[("reggaeton", 1.0)]);
        assert_eq!(cosine(&a, &b), 0.0);
        let c = v(&[("rock", 1.0)]);
        assert!((cosine(&a, &c) - 0.8).abs() < 1e-9);
        assert_eq!(cosine(&a, &TasteVector::new()), 0.0);
        assert!(neighbor_score(0.5, 100) > neighbor_score(0.5, 10));
        assert!((neighbor_score(0.5, 1000) - (0.5 + SHARED_BONUS)).abs() < 1e-9);

        let mix = blend(&[(1.0, &a), (1.0, &b)]);
        assert!(mix["reggaeton"] > mix["indie"]);
        assert!(mix.contains_key("rock"));
    }

    #[tokio::test]
    async fn job_computes_vectors_and_neighbors() {
        let store = Store::open_in_memory().await.unwrap();
        let tag = |t: &str| Tag {
            tag: t.into(),
            weight: 1.0,
            source: TagSource::Lastfm,
        };
        // 1 and 2 like rock (and share a track); 3 likes salsa; 4 likes rock but turned history off.
        for (user, keys) in [
            (1u64, &["r1", "r2"][..]),
            (2, &["r1", "r3"]),
            (3, &["s1"]),
            (4, &["r2"]),
        ] {
            for k in keys {
                let id = store.record_play(user, &track(k)).await.unwrap().unwrap();
                let g = if k.starts_with('r') { "rock" } else { "salsa" };
                store.write_track_tags(id, &[tag(g)]).await.unwrap();
            }
        }
        store
            .set_feedback(1, &track("s1"), Some(Feedback::Dislike))
            .await
            .unwrap();
        store
            .update_user_prefs(
                4,
                &UserPrefsPatch {
                    locale: None,
                    history_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let stop = CancellationToken::new();
        let users = recompute_stale(&store, &stop).await.unwrap();
        assert_eq!(
            users.iter().map(|u| u.0).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "history-off user skipped"
        );
        let t1 = store.get_taste(1).await.unwrap().unwrap().vector;
        assert!(t1["rock"] > 0.0 && t1["salsa"] < 0.0, "{t1:?}");
        assert!(store.stale_tastes(10).await.unwrap().is_empty());

        update_neighbors(&store, &users, &stop).await.unwrap();
        let n1 = store.neighbors(1, 10).await.unwrap();
        assert_eq!(n1.len(), 1);
        assert_eq!(n1[0].user_id, 2);
        // rock 0.8 / salsa -0.6 against pure rock: cosine 0.8, plus the shared-track bonus.
        assert!(n1[0].score > 0.8 && n1[0].score < 0.9, "{}", n1[0].score);
        assert!(store.neighbors(3, 10).await.unwrap().is_empty());
    }
}
