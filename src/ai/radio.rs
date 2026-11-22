//! One radio job: taste -> candidates -> early pick -> Ollama pick/order -> YouTube resolution.
//!
//! Every await is cancellable; the Ollama permit is a local that is dropped as soon as the job
//! leaves the model phase (or is cancelled), so the GPU slot is never held by a dead job.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    sync::{oneshot, watch},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::{
    candidates::{self, cid, Candidate, Filters, Source},
    lastfm::LastFm,
    ollama::{Ollama, OllamaPermit},
    queries, tagger,
    taste::{self, compute_vector},
    taxonomy::{self, GENRES},
    AiPhase, ApiError, Cancelled, EngineInner, RadioError, RadioPick, RadioRequest, Reason, ReasonKey,
};
use crate::{
    sources::{matcher, text::normalize, TrackInfo, TrackOrigin},
    store::{now_secs, track_key, CatalogTrack, Store, TasteVector, TrackId},
};

/// Owners with fewer distinct tracks than this get the cold-start mix.
pub const COLD_START_PLAYS: u32 = 10;
/// Last.fm suggestions resolved on YouTube per job (each is a search).
pub const MAX_RESOLVE: usize = 12;
const RESOLVE_CONCURRENCY: usize = 4;
const RESOLVE_NS: &str = "lastfm_yt";
const POPULAR_NS: &str = "ai_popular";
const POPULAR_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
const MIN_DURATION_MS: u64 = 45_000;
const MAX_DURATION_MS: u64 = 15 * 60_000;
/// Listeners whose taste is blended in group mode (big channels: the first ones).
const MAX_BLEND: usize = 12;
const FAVORITE_HALF_LIFE_DAYS: f64 = 60.0;

fn set(tx: &watch::Sender<AiPhase>, p: AiPhase) {
    tx.send_if_modified(|cur| {
        let changed = *cur != p;
        *cur = p;
        changed
    });
}

async fn guard<T>(c: &CancellationToken, fut: impl std::future::Future<Output = T>) -> Result<T, Cancelled> {
    tokio::select! {
        r = fut => Ok(r),
        _ = c.cancelled() => Err(Cancelled),
    }
}

pub(crate) async fn run(
    e: &EngineInner,
    req: &RadioRequest,
    c: &CancellationToken,
    phase: &watch::Sender<AiPhase>,
    mut first: Option<oneshot::Sender<RadioPick>>,
) -> Result<Vec<RadioPick>, RadioError> {
    if !e.settings.enabled {
        return Err(RadioError::Disabled);
    }
    let store = &e.store;
    if !store.history_enabled(req.owner_id).await? {
        return Err(RadioError::HistoryDisabled);
    }
    set(phase, AiPhase::Queued);
    let _slot = guard(c, e.radio_slots.clone().acquire_owned())
        .await?
        .map_err(|_| RadioError::Cancelled)?;

    // --- Taste ---------------------------------------------------------------------------
    set(phase, AiPhase::ReadingTaste);
    let n = req.batch_size.clamp(1, 50);
    let (everyone, members) = members(store, req).await?;
    let disliked = store.disliked_tracks(&everyone).await?;
    let recent: HashSet<String> = req.recent_keys.iter().cloned().collect();
    let mut vectors = Vec::with_capacity(members.len());
    for m in &members {
        vectors.push(member_taste(store, *m).await?);
    }
    let parts: Vec<(f64, &TasteVector)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| (if i == 0 { 1.0 } else { 0.7 }, v))
        .collect();
    let taste = taste::blend(&parts);
    let cold = store.history_len(req.owner_id).await? < COLD_START_PLAYS;
    let prompt = req.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let mut prompt_genres: Vec<&'static str> = prompt.map(taxonomy::genres_in_text).unwrap_or_default();
    if c.is_cancelled() {
        return Err(RadioError::Cancelled);
    }

    // --- Local candidates and the early pick -------------------------------------------
    set(phase, AiPhase::FindingSongs);
    let filters = Filters {
        disliked: &disliked,
        recent_keys: &recent,
        min_duration_ms: MIN_DURATION_MS,
        max_duration_ms: MAX_DURATION_MS,
    };
    let quotas = candidates::quotas(prompt.is_some(), cold);
    let mut cands = local_candidates(e, req, &members, cold, prompt, &prompt_genres).await?;
    let mut excluded_idents: HashSet<String> = HashSet::new();
    let mut first_sent = false;
    rescore(&mut cands, &taste, &prompt_genres, cold, prompt.is_some());
    cands = candidates::filter_dedupe(cands, &filters);
    if let Some(tx) = first.take() {
        let playable: Vec<Candidate> = cands.iter().filter(|c| c.info.is_some()).cloned().collect();
        match candidates::interleave(playable, &quotas).into_iter().next() {
            Some(top) => {
                excluded_idents.insert(top.ident());
                let _ = tx.send(to_pick(&top));
                first_sent = true;
                cands.retain(|c| c.ident() != top.ident());
            }
            None => first = Some(tx),
        }
    }

    // --- Prompt interpretation (model) ----------------------------------------------------
    let mut intent = None;
    if let (Some(p), Some(o)) = (prompt, &e.ollama) {
        if let Some(permit) = gpu(o, c, phase).await? {
            set(phase, AiPhase::Thinking);
            match interpret(o, &permit, p, c).await {
                Ok(i) => intent = Some(i),
                Err(ApiError::Cancelled) => return Err(RadioError::Cancelled),
                Err(err) => debug!(error = %err, "prompt interpretation failed"),
            }
            drop(permit);
            set(phase, AiPhase::FindingSongs);
        }
    }
    if let Some(i) = &intent {
        let new: Vec<&'static str> = i
            .genres
            .iter()
            .filter_map(|g| taxonomy::canonical(g))
            .filter(|g| !prompt_genres.contains(g))
            .collect();
        if !new.is_empty() {
            let extra = popular_candidates(e, &new, prompt, 60).await?;
            prompt_genres.extend(new);
            cands.extend(extra);
        }
    }

    // --- Last.fm: similar to favorites, top tracks of the requested genres -----------------
    if let Some(lfm) = &e.lastfm {
        cands.extend(similar_candidates(e, lfm, req.owner_id, c).await?);
        if let Some(p) = prompt {
            cands.extend(genre_candidates(lfm, &prompt_genres, Reason::matches_prompt(p), c).await?);
        }
    }
    rescore(&mut cands, &taste, &prompt_genres, cold, prompt.is_some());
    cands = candidates::filter_dedupe(cands, &filters);
    cands.retain(|c| !excluded_idents.contains(&c.ident()));
    if cands.is_empty() {
        return if first_sent {
            Ok(Vec::new())
        } else {
            Err(RadioError::NoCandidates)
        };
    }
    let ordered = candidates::interleave(cands, &quotas);
    let pool = candidates::pool(ordered, (n * 2).clamp(12, 40), 3);

    // --- Model pick and order ------------------------------------------------------------
    let mut order: Vec<usize> = (0..pool.len()).collect();
    if let (Some(o), true) = (&e.ollama, pool.len() > 1) {
        if let Some(permit) = gpu(o, c, phase).await? {
            set(phase, AiPhase::Thinking);
            match pick_with_model(o, &permit, &pool, n, prompt, intent.as_ref(), &taste, c).await {
                Ok(ids) => {
                    let valid = candidates::validate_picks(&ids, pool.len(), n);
                    match candidates::merge_model_order(&valid, pool.len(), pool.len(), (n / 2).max(1)) {
                        Some(m) => order = m,
                        None => debug!(valid = valid.len(), "model picks rejected, using the algorithmic order"),
                    }
                }
                Err(ApiError::Cancelled) => return Err(RadioError::Cancelled),
                Err(err) => debug!(error = %err, "model pick failed, using the algorithmic order"),
            }
            drop(permit);
        }
    }
    let ordered: Vec<Candidate> = order.into_iter().map(|i| pool[i].clone()).collect();
    let ordered = candidates::enforce_mix(ordered, &quotas, n);
    let ordered = candidates::diversify(ordered, 2, 10);

    // --- Resolve on YouTube ----------------------------------------------------------------
    set(phase, AiPhase::Resolving);
    let mut picks = resolve(e, ordered, n + usize::from(first.is_some()), &filters, c).await?;
    if c.is_cancelled() {
        return Err(RadioError::Cancelled);
    }
    if let Some(tx) = first {
        if picks.is_empty() {
            return Err(RadioError::NoCandidates);
        }
        let _ = tx.send(picks.remove(0));
    }
    picks.truncate(n);
    Ok(picks)
}

/// (everyone listening incl. the owner, members whose taste counts: owner first, history on).
async fn members(store: &Store, req: &RadioRequest) -> anyhow::Result<(Vec<u64>, Vec<u64>)> {
    let mut everyone = vec![req.owner_id];
    for l in &req.listener_ids {
        if !everyone.contains(l) {
            everyone.push(*l);
        }
    }
    let mut members = vec![req.owner_id];
    for l in everyone.iter().skip(1) {
        if members.len() >= MAX_BLEND {
            break;
        }
        if store.history_enabled(*l).await? {
            members.push(*l);
        }
    }
    Ok((everyone, members))
}

/// Stored vector, or computed on the fly when the background job has not run yet.
async fn member_taste(store: &Store, user: u64) -> anyhow::Result<TasteVector> {
    if let Some(t) = store.get_taste(user).await? {
        if !t.vector.is_empty() {
            return Ok(t.vector);
        }
    }
    Ok(compute_vector(&store.taste_inputs(user).await?, now_secs()))
}

fn prompt_vector(genres: &[&'static str]) -> TasteVector {
    genres
        .iter()
        .enumerate()
        .map(|(i, g)| (g.to_string(), 1.0 / (1.0 + i as f64 * 0.3)))
        .collect()
}

fn rescore(cands: &mut [Candidate], taste: &TasteVector, prompt_genres: &[&'static str], cold: bool, prompt: bool) {
    let pv = prompt_vector(prompt_genres);
    let target = match (pv.is_empty(), cold) {
        (true, _) => taste.clone(),
        (false, true) => taste::blend(&[(1.0, &pv), (0.3, taste)]),
        (false, false) => taste::blend(&[(1.5, &pv), (1.0, taste)]),
    };
    candidates::normalize_bases(cands);
    candidates::score_all(cands, &target, if prompt { 1.0 } else { 0.0 });
}

fn from_catalog(t: CatalogTrack, source: Source, base: f64, reason: Reason) -> Option<Candidate> {
    let info = t.info?;
    let artist = t
        .artists
        .first()
        .cloned()
        .or_else(|| info.artists.first().cloned())
        .unwrap_or_default();
    Some(Candidate {
        source,
        track_id: Some(t.id),
        key: Some(t.key),
        title: t.title,
        artist,
        duration_ms: t.duration_ms.or(info.duration_ms),
        info: Some(info),
        genres: Vec::new(),
        base,
        score: base,
        reason,
    })
}

fn recency(last_played: i64, now: i64, half_life_days: f64) -> f64 {
    0.5f64.powf(((now - last_played).max(0) as f64 / 86_400.0) / half_life_days)
}

async fn local_candidates(
    e: &EngineInner,
    req: &RadioRequest,
    members: &[u64],
    cold: bool,
    prompt: Option<&str>,
    prompt_genres: &[&'static str],
) -> Result<Vec<Candidate>, RadioError> {
    let store = &e.store;
    let now = now_secs();
    let mut out = Vec::new();

    // Favorites: most played, recently played, liked.
    let liked: Vec<TrackId> = store.liked_tracks(req.owner_id, 50).await?;
    let liked_set: HashSet<TrackId> = liked.iter().copied().collect();
    let mut in_history = HashSet::new();
    for (i, m) in members.iter().enumerate() {
        for h in store.history_entries(*m, if i == 0 { 80 } else { 30 }).await? {
            in_history.insert(h.track.id);
            let mut base = (1.0 + f64::from(h.play_count).ln()) * recency(h.last_played, now, FAVORITE_HALF_LIFE_DAYS);
            if liked_set.contains(&h.track.id) {
                base *= 1.5;
            }
            if i > 0 {
                base *= 0.7;
            }
            out.extend(from_catalog(
                h.track,
                Source::Favorite,
                base,
                Reason::new(ReasonKey::YourFavorite),
            ));
        }
    }
    let liked_only: Vec<TrackId> = liked.into_iter().filter(|id| !in_history.contains(id)).collect();
    for t in queries::catalog_tracks(store, &liked_only).await? {
        out.extend(from_catalog(
            t,
            Source::Favorite,
            2.0,
            Reason::new(ReasonKey::YourFavorite),
        ));
    }

    // Discover: what similar listeners play.
    for m in members {
        for d in store.discover_tracks(*m, 60).await? {
            out.extend(from_catalog(
                d.track,
                Source::Discover,
                d.score,
                Reason::new(ReasonKey::SimilarListeners),
            ));
        }
    }

    // Prompt genres, or cold start: popular in the chosen genres (or overall).
    if prompt.is_some() || cold {
        let genres: Vec<&'static str> = if !prompt_genres.is_empty() || prompt.is_some() {
            prompt_genres.to_vec()
        } else {
            store
                .top_genres(req.owner_id, 5)
                .await?
                .iter()
                .filter_map(|g| taxonomy::canonical(&g.genre))
                .collect()
        };
        out.extend(popular_candidates(e, &genres, prompt, 80).await?);
    }

    attach_genres(store, &mut out).await?;
    Ok(out)
}

async fn attach_genres(store: &Store, cands: &mut [Candidate]) -> anyhow::Result<()> {
    let ids: Vec<TrackId> = cands
        .iter()
        .filter(|c| c.genres.is_empty())
        .filter_map(|c| c.track_id)
        .collect();
    let tags = queries::tags_for_tracks(store, &ids).await?;
    for c in cands.iter_mut() {
        if let Some(t) = c.track_id.and_then(|id| tags.get(&id)) {
            c.genres = t.clone();
        }
    }
    Ok(())
}

/// Popular tracks in `genres` (overall when empty), cached for an hour across all guilds.
async fn popular_candidates(
    e: &EngineInner,
    genres: &[&'static str],
    prompt: Option<&str>,
    limit: u32,
) -> Result<Vec<Candidate>, RadioError> {
    let store = &e.store;
    let mut sorted: Vec<&str> = genres.to_vec();
    sorted.sort_unstable();
    let key = format!("{}|{limit}", sorted.join(","));
    let ranked: Vec<(TrackId, f64)> = match store.cache_get::<Vec<(TrackId, f64)>>(POPULAR_NS, &key).await? {
        Some(hit) => hit,
        None => {
            let rows = queries::popular_in_genres(store, genres, limit).await?;
            let ids: Vec<(TrackId, f64)> = rows.iter().map(|(t, s)| (t.id, *s)).collect();
            let _ = store.cache_put(POPULAR_NS, &key, &ids, POPULAR_TTL).await;
            ids
        }
    };
    let score: HashMap<TrackId, f64> = ranked.iter().copied().collect();
    let ids: Vec<TrackId> = ranked.iter().map(|(id, _)| *id).collect();
    let reason = match prompt {
        Some(p) => Reason::matches_prompt(p),
        None => Reason::new(ReasonKey::SimilarListeners),
    };
    let mut out: Vec<Candidate> = queries::catalog_tracks(store, &ids)
        .await?
        .into_iter()
        .filter_map(|t| {
            let s = score.get(&t.id).copied().unwrap_or(0.0);
            from_catalog(t, Source::Prompt, s, reason.clone())
        })
        .collect();
    attach_genres(store, &mut out).await?;
    Ok(out)
}

/// Last.fm suggestions from the owner's top tracks, likes and top artist. Unresolved (no YouTube
/// id yet); only the ones that make the final cut get resolved.
async fn similar_candidates(
    e: &EngineInner,
    lfm: &LastFm,
    owner: u64,
    c: &CancellationToken,
) -> Result<Vec<Candidate>, RadioError> {
    let store = &e.store;
    let top = store.history_entries(owner, 10).await?;
    let mut seeds: Vec<CatalogTrack> = top.iter().take(3).map(|h| h.track.clone()).collect();
    let liked = store.liked_tracks(owner, 5).await?;
    for t in queries::catalog_tracks(store, &liked).await? {
        if seeds.len() >= 5 {
            break;
        }
        if !seeds.iter().any(|s| s.id == t.id) {
            seeds.push(t);
        }
    }
    let mut out = Vec::new();
    for seed in &seeds {
        let (artist, title) = tagger::lookup_names(seed);
        if artist.is_empty() || title.is_empty() {
            continue;
        }
        for s in skip(lfm.similar_tracks(&artist, &title, 8, c).await)? {
            out.push(lfm_candidate(
                s.artist,
                s.title,
                s.duration_ms,
                s.score,
                Reason::because_you_like(&artist),
            ));
        }
    }
    // The owner's most frequent artist -> similar artists -> their top tracks.
    let mut counts: HashMap<String, (usize, String)> = HashMap::new();
    for h in &top {
        let (artist, _) = tagger::lookup_names(&h.track);
        if !artist.is_empty() {
            counts.entry(normalize(&artist)).or_insert((0, artist)).0 += 1;
        }
    }
    if let Some((_, (_, artist))) = counts
        .into_iter()
        .max_by_key(|(k, (n, _))| (*n, std::cmp::Reverse(k.clone())))
    {
        for (sim, m) in skip(lfm.similar_artists(&artist, 4, c).await)? {
            for t in skip(lfm.artist_top_tracks(&sim, 2, c).await)? {
                out.push(lfm_candidate(
                    t.artist,
                    t.title,
                    t.duration_ms,
                    m * (0.5 + 0.5 * t.score),
                    Reason::because_you_like(&artist),
                ));
            }
        }
    }
    // Genres from the artist cache, when the tagger already saw these artists.
    for cand in out.iter_mut() {
        if let Ok(Some(tags)) = store.artist_tags(&cand.artist, e.settings.cache_ttl).await {
            cand.genres = tags.into_iter().map(|t| (t.tag, t.weight)).collect();
        }
    }
    Ok(out)
}

/// Popular Last.fm tracks for the first requested genres (unresolved, like the similar ones).
async fn genre_candidates(
    lfm: &LastFm,
    genres: &[&'static str],
    reason: Reason,
    c: &CancellationToken,
) -> Result<Vec<Candidate>, RadioError> {
    let mut out = Vec::new();
    for (i, g) in genres.iter().take(3).enumerate() {
        let weight = 1.0 - 0.2 * i as f64;
        for t in skip(lfm.tag_top_tracks(taxonomy::lastfm_tag(g), 25, c).await)? {
            let mut cand = lfm_candidate(
                t.artist,
                t.title,
                t.duration_ms,
                weight * (0.5 + 0.5 * t.score),
                reason.clone(),
            );
            cand.source = Source::Prompt;
            cand.genres = vec![((*g).to_owned(), 1.0)];
            out.push(cand);
        }
    }
    Ok(out)
}

/// Last.fm errors only drop that suggestion source; cancellation ends the job.
fn skip<T>(r: Result<Vec<T>, ApiError>) -> Result<Vec<T>, RadioError> {
    match r {
        Ok(v) => Ok(v),
        Err(ApiError::Cancelled) => Err(RadioError::Cancelled),
        Err(err) => {
            debug!(error = %err, "Last.fm lookup failed");
            Ok(Vec::new())
        }
    }
}

fn lfm_candidate(artist: String, title: String, duration_ms: Option<u64>, score: f64, reason: Reason) -> Candidate {
    Candidate {
        source: Source::Similar,
        track_id: None,
        key: None,
        info: None,
        title,
        artist,
        duration_ms,
        genres: Vec::new(),
        base: score,
        score,
        reason,
    }
}

/// Wait for the GPU and make sure the model is loaded. None: Ollama is unreachable, use the algorithm.
async fn gpu(
    o: &Ollama,
    c: &CancellationToken,
    phase: &watch::Sender<AiPhase>,
) -> Result<Option<OllamaPermit>, RadioError> {
    let permit = match o.try_acquire() {
        Some(p) => p,
        None => {
            set(phase, AiPhase::Queued);
            o.acquire(c).await?
        }
    };
    match guard(c, o.is_loaded()).await? {
        None => Ok(None),
        Some(true) => Ok(Some(permit)),
        Some(false) => {
            set(phase, AiPhase::LoadingModel);
            match o.warm_up(&permit, c).await {
                Ok(took) => {
                    debug!(ms = took.as_millis() as u64, "model loaded");
                    Ok(Some(permit))
                }
                Err(ApiError::Cancelled) => Err(RadioError::Cancelled),
                Err(err) => {
                    debug!(error = %err, "model load failed");
                    Ok(None)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Intent {
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub mood: String,
    #[serde(default)]
    pub tempo: String,
    #[serde(default)]
    pub era: String,
}

async fn interpret(o: &Ollama, permit: &OllamaPermit, prompt: &str, c: &CancellationToken) -> Result<Intent, ApiError> {
    let schema = json!({
        "type": "object",
        "properties": {
            "genres": { "type": "array", "items": { "type": "string", "enum": GENRES }, "minItems": 1, "maxItems": 4 },
            "mood": { "type": "string" },
            "tempo": { "type": "string", "enum": ["slow", "medium", "fast", "any"] },
            "era": { "type": "string" }
        },
        "required": ["genres", "mood", "tempo", "era"]
    });
    let user = format!(
        "Music request (any language): {}\nChoose 1 to 4 genres from the allowed list that fit it best, the mood in 1 to 3 \
         English words, the tempo, and the era (like \"2010s\", \"80s\" or \"any\").",
        serde_json::to_string(prompt).unwrap_or_default()
    );
    o.chat_json(
        permit,
        "You turn music requests into search parameters. Answer only with JSON.",
        &user,
        &schema,
        c,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn pick_with_model(
    o: &Ollama,
    permit: &OllamaPermit,
    pool: &[Candidate],
    n: usize,
    prompt: Option<&str>,
    intent: Option<&Intent>,
    taste: &TasteVector,
    c: &CancellationToken,
) -> Result<Vec<String>, ApiError> {
    #[derive(Deserialize)]
    struct Resp {
        ids: Vec<String>,
    }
    let ids: Vec<String> = (0..pool.len()).map(cid).collect();
    let list: Vec<serde_json::Value> = pool
        .iter()
        .enumerate()
        .map(|(i, c)| {
            json!({
                "id": ids[i],
                "artist": c.artist,
                "title": c.title,
                "genres": c.genres.iter().take(3).map(|g| g.0.as_str()).collect::<Vec<_>>(),
                "source": match c.source {
                    Source::Discover => "similar listeners",
                    Source::Similar => "similar to favorites",
                    Source::Favorite => "favorite",
                    Source::Prompt => "matches request",
                },
            })
        })
        .collect();
    let mut top: Vec<(&String, &f64)> = taste.iter().filter(|(_, w)| **w > 0.0).collect();
    top.sort_by(|a, b| b.1.total_cmp(a.1));
    let context = json!({
        "request": prompt,
        "mood": intent.map(|i| i.mood.as_str()),
        "tempo": intent.map(|i| i.tempo.as_str()),
        "era": intent.map(|i| i.era.as_str()),
        "listener_genres": top.iter().take(8).map(|(g, _)| g.as_str()).collect::<Vec<_>>(),
        "count": n,
        "candidates": list,
    });
    let schema = json!({
        "type": "object",
        "properties": { "ids": { "type": "array", "items": { "type": "string", "enum": ids }, "minItems": 1, "maxItems": n } },
        "required": ["ids"]
    });
    let system = "You are a radio DJ. Pick songs ONLY from the candidate list, by id, and order them so the session \
                  flows (energy and genre transitions, no same artist back to back). Follow the listener's request \
                  when there is one. Never invent ids. Answer only with JSON.";
    let user = format!("Pick {n} songs and order them.\n{context}");
    let r: Resp = o.chat_json(permit, system, &user, &schema, c).await?;
    Ok(r.ids)
}

/// Resolve the unresolved candidates near the top (bounded), re-check filters on the resolved
/// keys and return up to `n` playable picks, keeping the order and the artist rule.
async fn resolve(
    e: &EngineInner,
    ordered: Vec<Candidate>,
    n: usize,
    filters: &Filters<'_>,
    c: &CancellationToken,
) -> Result<Vec<RadioPick>, RadioError> {
    // Unresolved candidates that could make the cut, best first.
    let mut want: Vec<usize> = Vec::new();
    let mut playable = 0usize;
    for (i, cand) in ordered.iter().enumerate() {
        if playable + want.len() >= n + 4 {
            break;
        }
        if cand.info.is_some() {
            playable += 1;
        } else if want.len() < MAX_RESOLVE {
            want.push(i);
        }
    }
    let mut resolved: HashMap<usize, Option<TrackInfo>> = HashMap::new();
    for chunk in want.chunks(RESOLVE_CONCURRENCY) {
        let mut set = JoinSet::new();
        for &i in chunk {
            let cand = &ordered[i];
            let (store, sources, ttl) = (e.store.clone(), e.sources.clone(), e.settings.cache_ttl);
            let (artist, title, dur) = (cand.artist.clone(), cand.title.clone(), cand.duration_ms);
            set.spawn(async move { (i, resolve_one(&store, &sources, ttl, &artist, &title, dur).await) });
        }
        let results = guard(c, async {
            let mut v = Vec::new();
            while let Some(r) = set.join_next().await {
                if let Ok(x) = r {
                    v.push(x);
                }
            }
            v
        })
        .await?;
        resolved.extend(results);
    }

    let mut out: Vec<Candidate> = Vec::new();
    let mut keys: HashSet<String> = HashSet::new();
    for (i, mut cand) in ordered.into_iter().enumerate() {
        if cand.info.is_none() {
            let Some(Some(info)) = resolved.remove(&i) else {
                continue;
            };
            let key = track_key(&info);
            cand.track_id = e.store.track_id(&key).await?;
            cand.key = Some(key);
            cand.duration_ms = cand.duration_ms.or(info.duration_ms);
            cand.info = Some(info);
            if !filters.allows(&cand) {
                continue;
            }
        }
        if cand.key.as_ref().is_some_and(|k| !keys.insert(k.clone())) {
            continue;
        }
        out.push(cand);
    }
    let out = candidates::diversify(out, 2, 10);
    let picks: Vec<RadioPick> = out.iter().take(n).map(to_pick).collect();
    // New tracks enter the catalog so the tagger picks them up.
    let mut added = false;
    for p in &picks {
        if p.genres.is_empty() && e.store.ensure_track(&p.track).await.is_ok() {
            added = true;
        }
    }
    if added {
        e.tagger_wake.notify_one();
    }
    Ok(picks)
}

/// Last.fm (artist, title) -> YouTube track, cached (misses too).
async fn resolve_one(
    store: &Store,
    sources: &crate::sources::Sources,
    ttl: std::time::Duration,
    artist: &str,
    title: &str,
    duration_ms: Option<u64>,
) -> Option<TrackInfo> {
    let key = format!("{}|{}", normalize(artist), normalize(title));
    if let Ok(Some(hit)) = store.cache_get::<Option<TrackInfo>>(RESOLVE_NS, &key).await {
        return hit;
    }
    let src = TrackInfo {
        title: title.to_owned(),
        artists: vec![artist.to_owned()],
        duration_ms,
        thumbnail: None,
        url: String::new(),
        origin: TrackOrigin::Spotify,
        youtube_id: None,
        spotify_id: None,
        isrc: None,
    };
    let results = sources.search(&matcher::query_for(&src), 8).await.ok()?;
    let cands: Vec<matcher::Candidate> = results.into_iter().map(matcher::Candidate::new).collect();
    let best = matcher::best(&src, &cands).map(|(c, _)| c.track.clone());
    let _ = store.cache_put(RESOLVE_NS, &key, &best, ttl).await;
    best
}

fn to_pick(c: &Candidate) -> RadioPick {
    RadioPick {
        track: c.info.clone().expect("only playable candidates become picks"),
        reason: c.reason.clone(),
        genres: c.genres.iter().take(3).map(|g| g.0.clone()).collect(),
    }
}

#[cfg(test)]
mod tests;
