//! Radio candidates: filtering, scoring, the source mix, artist diversity and validation of the
//! model's picks. Pure functions, no I/O.

use std::collections::{HashMap, HashSet, VecDeque};

use super::{tagger::clean_channel, taste::cosine, RadioMix, Reason};
use crate::{
    sources::{text::normalize, TrackInfo},
    store::{normalize_artist, TasteVector, TrackId},
};

/// Where a candidate came from (drives the mix and the "why this song" reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// Frequent with similar listeners, never played by the user.
    Discover,
    /// Last.fm similar tracks / artists of the user's favorites.
    Similar,
    /// The user's own most played and liked tracks.
    Favorite,
    /// Popular tracks in the genres of the prompt (or the user's genres on cold start).
    Prompt,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub source: Source,
    pub track_id: Option<TrackId>,
    pub key: Option<String>,
    /// Playable metadata; None for Last.fm suggestions until resolved on YouTube.
    pub info: Option<TrackInfo>,
    pub title: String,
    pub artist: String,
    pub duration_ms: Option<u64>,
    pub genres: Vec<(String, f64)>,
    /// Source-specific strength, 0..1 within its source.
    pub base: f64,
    /// Final algorithmic score.
    pub score: f64,
    pub reason: Reason,
}

impl Candidate {
    pub fn artist_key(&self) -> String {
        normalize_artist(&clean_channel(&self.artist))
    }

    /// Identity across sources: same song even with different keys.
    pub fn ident(&self) -> String {
        format!("{}|{}", normalize(&clean_channel(&self.artist)), normalize(&self.title))
    }

    pub fn genre_vector(&self) -> TasteVector {
        self.genres.iter().cloned().collect()
    }
}

pub struct Filters<'a> {
    pub disliked: &'a HashSet<TrackId>,
    pub recent_keys: &'a HashSet<String>,
    pub min_duration_ms: u64,
    pub max_duration_ms: u64,
}

impl Filters<'_> {
    pub fn allows(&self, c: &Candidate) -> bool {
        if c.track_id.is_some_and(|id| self.disliked.contains(&id)) {
            return false;
        }
        if c.key.as_ref().is_some_and(|k| self.recent_keys.contains(k)) {
            return false;
        }
        if let Some(d) = c.duration_ms {
            if d < self.min_duration_ms || d > self.max_duration_ms {
                return false;
            }
        }
        !c.title.trim().is_empty()
    }
}

/// Drop filtered candidates and duplicates (same key or same artist+title), keeping the best score.
pub fn filter_dedupe(mut cands: Vec<Candidate>, f: &Filters) -> Vec<Candidate> {
    cands.retain(|c| f.allows(c));
    cands.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut keys = HashSet::new();
    let mut idents = HashSet::new();
    cands.retain(|c| {
        let new_key = c.key.as_ref().is_none_or(|k| keys.insert(k.clone()));
        new_key && idents.insert(c.ident())
    });
    cands
}

/// Normalize `base` within each source so the best of each source is 1.0.
pub fn normalize_bases(cands: &mut [Candidate]) {
    let mut max: HashMap<Source, f64> = HashMap::new();
    for c in cands.iter() {
        let m = max.entry(c.source).or_insert(0.0);
        *m = m.max(c.base);
    }
    for c in cands.iter_mut() {
        let m = max[&c.source];
        c.base = if m > 0.0 { (c.base / m).clamp(0.0, 1.0) } else { 0.0 };
    }
}

/// Final score: source strength plus genre affinity with the target vector. Unknown genres get a
/// neutral affinity so untagged tracks are not starved. `prompt_weight` (0..1) shifts weight to
/// affinity when the user asked for something specific.
pub fn score_all(cands: &mut [Candidate], target: &TasteVector, prompt_weight: f64) {
    let aff_w = 0.45 + 0.3 * prompt_weight.clamp(0.0, 1.0);
    for c in cands.iter_mut() {
        let affinity = if c.genres.is_empty() || target.is_empty() {
            0.25
        } else {
            cosine(&c.genre_vector(), target).max(-0.5)
        };
        c.score = (1.0 - aff_w) * c.base + aff_w * affinity;
    }
}

/// Share of each source in a batch.
pub fn quotas(prompt: bool, cold_start: bool) -> Vec<(Source, f64)> {
    match (prompt, cold_start) {
        (_, true) => vec![
            (Source::Prompt, 0.7),
            (Source::Favorite, 0.2),
            (Source::Similar, 0.1),
            (Source::Discover, 0.0),
        ],
        (true, false) => {
            vec![
                (Source::Prompt, 0.35),
                (Source::Discover, 0.3),
                (Source::Similar, 0.2),
                (Source::Favorite, 0.15),
            ]
        }
        (false, false) => {
            vec![
                (Source::Discover, 0.5),
                (Source::Similar, 0.3),
                (Source::Favorite, 0.2),
                (Source::Prompt, 0.0),
            ]
        }
    }
}

/// Apply the `/ai` "Mix" to the source shares: Favorites leans on the user's own tracks, Discover
/// on tracks they never played. The prompt share is left alone; the result sums to 1 again.
pub fn with_mix(quotas: Vec<(Source, f64)>, mix: RadioMix) -> Vec<(Source, f64)> {
    let factor = |s: Source| match (mix, s) {
        (RadioMix::Balanced, _) | (_, Source::Prompt) => 1.0,
        (RadioMix::Favorites, Source::Favorite) => 3.0,
        (RadioMix::Favorites, Source::Similar) => 0.8,
        (RadioMix::Favorites, Source::Discover) => 0.3,
        (RadioMix::Discover, Source::Discover) => 1.5,
        (RadioMix::Discover, Source::Similar) => 1.2,
        (RadioMix::Discover, Source::Favorite) => 0.25,
    };
    let before: f64 = quotas.iter().map(|(_, q)| q).sum();
    let scaled: Vec<(Source, f64)> = quotas.into_iter().map(|(s, q)| (s, q * factor(s))).collect();
    let after: f64 = scaled.iter().map(|(_, q)| q).sum();
    if after <= 0.0 {
        return scaled;
    }
    scaled.into_iter().map(|(s, q)| (s, q * before / after)).collect()
}

/// Algorithmic order: interleave sources by quota (largest deficit first, best score within a
/// source), then fill with whatever is left. Sources with no candidates hand their share over.
pub fn interleave(cands: Vec<Candidate>, quotas: &[(Source, f64)]) -> Vec<Candidate> {
    let mut queues: HashMap<Source, VecDeque<Candidate>> = HashMap::new();
    let mut sorted = cands;
    sorted.sort_by(|a, b| b.score.total_cmp(&a.score));
    for c in sorted {
        queues.entry(c.source).or_default().push_back(c);
    }
    let mut placed: HashMap<Source, f64> = HashMap::new();
    let mut out = Vec::new();
    loop {
        let total = out.len() as f64 + 1.0;
        let pick = quotas
            .iter()
            .filter(|(s, q)| *q > 0.0 && queues.get(s).is_some_and(|q| !q.is_empty()))
            .max_by(|a, b| {
                let da = a.1 * total - placed.get(&a.0).copied().unwrap_or(0.0);
                let db = b.1 * total - placed.get(&b.0).copied().unwrap_or(0.0);
                da.total_cmp(&db)
            })
            .map(|(s, _)| *s);
        let Some(src) = pick else { break };
        if let Some(c) = queues.get_mut(&src).and_then(VecDeque::pop_front) {
            *placed.entry(src).or_insert(0.0) += 1.0;
            out.push(c);
        }
    }
    // Zero-quota sources and leftovers, best first.
    let mut rest: Vec<Candidate> = queues.into_values().flatten().collect();
    rest.sort_by(|a, b| b.score.total_cmp(&a.score));
    out.extend(rest);
    out
}

/// Keep the source mix after the model reordered the pool: at most ~1.5x each source's share of
/// `n` stays in place; the overflow moves to the end (used only when others fail to resolve).
pub fn enforce_mix(order: Vec<Candidate>, quotas: &[(Source, f64)], n: usize) -> Vec<Candidate> {
    let cap = |s: Source| {
        let q = quotas
            .iter()
            .find(|(x, _)| *x == s)
            .map(|x| x.1)
            .unwrap_or(0.0)
            .max(0.1);
        ((q * n as f64 * 1.5).ceil() as usize).max(1)
    };
    let mut count: HashMap<Source, usize> = HashMap::new();
    let mut out = Vec::with_capacity(order.len());
    let mut over = Vec::new();
    for c in order {
        let k = count.entry(c.source).or_insert(0);
        if *k < cap(c.source) {
            *k += 1;
            out.push(c);
        } else {
            over.push(c);
        }
    }
    out.extend(over);
    out
}

/// Cut a long ordered list to a pool: at most `per_artist` tracks per artist, `size` in total.
pub fn pool(ordered: Vec<Candidate>, size: usize, per_artist: usize) -> Vec<Candidate> {
    let mut count: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(size);
    for c in ordered {
        if out.len() >= size {
            break;
        }
        let n = count.entry(c.artist_key()).or_insert(0);
        if *n < per_artist {
            *n += 1;
            out.push(c);
        }
    }
    out
}

/// Reorder so that no artist appears more than `max` times in any `window` consecutive tracks,
/// keeping the input order as much as possible. Items that never fit are dropped.
pub fn diversify(list: Vec<Candidate>, max: usize, window: usize) -> Vec<Candidate> {
    let fits = |out: &[Candidate], c: &Candidate| {
        let start = out.len().saturating_sub(window.saturating_sub(1));
        let key = c.artist_key();
        out[start..].iter().filter(|x| x.artist_key() == key).count() < max
    };
    let mut out: Vec<Candidate> = Vec::with_capacity(list.len());
    let mut deferred: VecDeque<Candidate> = VecDeque::new();
    for c in list {
        // Deferred items get the first chance at every new position.
        while let Some(pos) = deferred.iter().position(|d| fits(&out, d)) {
            let d = deferred.remove(pos).expect("position is valid");
            out.push(d);
        }
        if fits(&out, &c) {
            out.push(c);
        } else {
            deferred.push_back(c);
        }
    }
    while let Some(pos) = deferred.iter().position(|d| fits(&out, d)) {
        let d = deferred.remove(pos).expect("position is valid");
        out.push(d);
    }
    out
}

/// Candidate id shown to the model.
pub fn cid(i: usize) -> String {
    format!("c{}", i + 1)
}

/// Turn the model's ids into pool indexes: unknown ids dropped, duplicates removed, capped at `n`.
pub fn validate_picks(ids: &[String], pool_len: usize, n: usize) -> Vec<usize> {
    let mut seen = HashSet::new();
    ids.iter()
        .filter_map(|id| {
            let id = id.trim();
            let num = id.strip_prefix('c').or_else(|| id.strip_prefix('C')).unwrap_or(id);
            let i: usize = num.parse().ok()?;
            (1..=pool_len).contains(&i).then(|| i - 1)
        })
        .filter(|i| seen.insert(*i))
        .take(n)
        .collect()
}

/// Final order from the model's picks: accepted when at least `min_valid` ids are valid, then
/// topped up from the algorithmic order. None means "use the algorithm".
pub fn merge_model_order(picks: &[usize], pool_len: usize, n: usize, min_valid: usize) -> Option<Vec<usize>> {
    if picks.is_empty() || picks.len() < min_valid.min(pool_len).max(1) {
        return None;
    }
    let mut out: Vec<usize> = picks.iter().copied().take(n).collect();
    let used: HashSet<usize> = out.iter().copied().collect();
    out.extend(
        (0..pool_len)
            .filter(|i| !used.contains(i))
            .take(n.saturating_sub(out.len())),
    );
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{ai::ReasonKey, store::test_util::track};

    #[test]
    fn mix_shifts_source_shares() {
        let base = quotas(false, false);
        assert_eq!(with_mix(base.clone(), RadioMix::Balanced), base);
        let share = |q: &[(Source, f64)], s: Source| q.iter().find(|(x, _)| *x == s).unwrap().1;
        let fav = with_mix(base.clone(), RadioMix::Favorites);
        let disc = with_mix(base.clone(), RadioMix::Discover);
        assert!(share(&fav, Source::Favorite) > 0.4, "{fav:?}");
        assert!(share(&fav, Source::Discover) < share(&base, Source::Discover));
        assert!(share(&disc, Source::Discover) > share(&base, Source::Discover));
        assert!(share(&disc, Source::Favorite) < 0.1);
        for q in [&fav, &disc] {
            let total: f64 = q.iter().map(|(_, v)| v).sum();
            assert!((total - 1.0).abs() < 1e-9);
        }
        // The prompt share is kept.
        let p = quotas(true, false);
        assert!(
            (share(&with_mix(p.clone(), RadioMix::Discover), Source::Prompt) - share(&p, Source::Prompt)).abs() < 0.05
        );
    }

    pub fn cand(key: &str, artist: &str, source: Source, score: f64) -> Candidate {
        let mut info = track(key);
        info.artists = vec![artist.into()];
        Candidate {
            source,
            track_id: Some(key.bytes().map(i64::from).sum()),
            key: Some(key.into()),
            title: format!("Song {key}"),
            artist: artist.into(),
            duration_ms: info.duration_ms,
            info: Some(info),
            genres: Vec::new(),
            base: score,
            score,
            reason: Reason::new(ReasonKey::YourFavorite),
        }
    }

    #[test]
    fn filters_dislikes_recent_duration_and_duplicates() {
        let mut a = cand("a", "X", Source::Discover, 0.9);
        let b = cand("b", "Y", Source::Discover, 0.8);
        let c = cand("c", "Z", Source::Favorite, 0.7);
        let mut d = cand("d", "W", Source::Similar, 0.6);
        d.duration_ms = Some(20_000);
        // Same song as b from another source (different key) with a lower score.
        let mut b2 = cand("b2", "Y", Source::Similar, 0.5);
        b2.title = "Song b".into();
        // Same key as c, lower score.
        let c2 = cand("c", "Z", Source::Discover, 0.1);
        let mut unresolved = cand("u", "V", Source::Similar, 0.4);
        unresolved.track_id = None;
        unresolved.key = None;
        a.track_id = Some(1);

        let disliked: HashSet<TrackId> = [1].into_iter().collect();
        let recent: HashSet<String> = ["c".to_string()].into_iter().collect();
        let f = Filters {
            disliked: &disliked,
            recent_keys: &recent,
            min_duration_ms: 45_000,
            max_duration_ms: 900_000,
        };
        let out = filter_dedupe(vec![a, b, c, d, b2, c2, unresolved], &f);
        let keys: Vec<_> = out.iter().map(|c| c.title.clone()).collect();
        assert_eq!(keys, vec!["Song b", "Song u"]);
    }

    #[test]
    fn diversity_max_two_per_ten() {
        let mut list = Vec::new();
        for i in 0..6 {
            list.push(cand(&format!("a{i}"), "Same Artist", Source::Favorite, 1.0));
        }
        for i in 0..12 {
            list.push(cand(&format!("o{i}"), &format!("Other {i}"), Source::Discover, 0.5));
        }
        let out = diversify(list, 2, 10);
        for w in out.windows(10) {
            assert!(w.iter().filter(|c| c.artist == "Same Artist").count() <= 2);
        }
        // Order kept where possible: the first two tracks are the top ones.
        assert_eq!(out[0].artist, "Same Artist");
        assert_eq!(out[1].artist, "Same Artist");
        assert!(out.len() >= 14);
        // "X - Topic" and "X" are the same artist.
        let t = cand("t", "Bad Bunny - Topic", Source::Favorite, 1.0);
        assert_eq!(
            t.artist_key(),
            cand("u", "bad bunny", Source::Favorite, 1.0).artist_key()
        );
    }

    #[test]
    fn interleave_follows_quotas() {
        let mut list = Vec::new();
        for i in 0..20 {
            list.push(cand(&format!("d{i}"), &format!("D{i}"), Source::Discover, 0.5));
            list.push(cand(&format!("s{i}"), &format!("S{i}"), Source::Similar, 0.9));
            list.push(cand(&format!("f{i}"), &format!("F{i}"), Source::Favorite, 0.7));
        }
        let out = interleave(list, &quotas(false, false));
        let first10 = &out[..10];
        let n = |s| first10.iter().filter(|c| c.source == s).count();
        assert_eq!(
            (n(Source::Discover), n(Source::Similar), n(Source::Favorite)),
            (5, 3, 2)
        );
        assert_eq!(out.len(), 60);

        // A missing source hands its share to the others.
        let only: Vec<_> = (0..5)
            .map(|i| cand(&format!("f{i}"), "F", Source::Favorite, 0.1))
            .collect();
        assert_eq!(interleave(only, &quotas(false, false)).len(), 5);
    }

    #[test]
    fn mix_is_kept_after_model_order() {
        // The model put 8 favorites first.
        let mut order: Vec<Candidate> = (0..8)
            .map(|i| cand(&format!("f{i}"), &format!("F{i}"), Source::Favorite, 1.0))
            .collect();
        order.extend((0..8).map(|i| cand(&format!("d{i}"), &format!("D{i}"), Source::Discover, 1.0)));
        let out = enforce_mix(order, &quotas(false, false), 10);
        // Favorites: 20% of 10 * 1.5 = 3 stay in front.
        let front: Vec<Source> = out[..11].iter().map(|c| c.source).collect();
        assert_eq!(front.iter().filter(|s| **s == Source::Favorite).count(), 3);
        assert_eq!(out.len(), 16);
        assert_eq!(out[0].key.as_deref(), Some("f0"));
    }

    #[test]
    fn scoring_prefers_genre_match() {
        let target: TasteVector = [("reggaeton".to_string(), 1.0)].into_iter().collect();
        let mut a = cand("a", "A", Source::Discover, 0.5);
        a.genres = vec![("reggaeton".into(), 1.0)];
        let mut b = cand("b", "B", Source::Discover, 0.5);
        b.genres = vec![("metal".into(), 1.0)];
        let c = cand("c", "C", Source::Discover, 0.5);
        let mut v = vec![a, b, c];
        score_all(&mut v, &target, 1.0);
        assert!(v[0].score > v[2].score && v[2].score > v[1].score);
    }

    #[test]
    fn model_pick_validation() {
        let ids: Vec<String> = ["c3", "zz", "c3", "C1", "c99", "2", ""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(validate_picks(&ids, 5, 10), vec![2, 0, 1]);
        assert_eq!(validate_picks(&ids, 5, 2), vec![2, 0]);
        assert!(validate_picks(&[], 5, 10).is_empty());

        // Enough valid ids: model order first, then the algorithmic order fills the batch.
        assert_eq!(merge_model_order(&[2, 0], 5, 4, 2), Some(vec![2, 0, 1, 3]));
        // Too few valid ids or none: fall back.
        assert_eq!(merge_model_order(&[2], 5, 4, 2), None);
        assert_eq!(merge_model_order(&[], 5, 4, 0), None);
        // Small pools lower the bar.
        assert_eq!(merge_model_order(&[0], 1, 4, 3), Some(vec![0]));
    }
}
