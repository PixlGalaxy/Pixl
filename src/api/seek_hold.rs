//! Seek-bar lock: while one person drags the panel's progress bar, the others in the same player
//! see it disabled ("<name> is moving the bar"), so two people don't fight over it.
//!
//! The panel sends `{"type":"seek_hold"}` on its socket when a drag starts and `seek_release` when
//! it ends; the seek itself still goes through `POST /api/player/seek`. A hold is granted to the
//! first user who asks (and may control the player); the holder may ask again (it is kept, not
//! extended). It lasts at most [`HOLD_MAX`] from its start; after that the server clears it even
//! when the pointer is still down, and the same user can't take it again for [`REACQUIRE_COOLDOWN`].
//! A hold also ends with its holder's seek, its socket closing (or following another channel), a
//! new current track and the player stopping.
//!
//! Scope: one hold per player channel, and one per jam (the timeline is shared, so a hold in any
//! member channel locks the bar in every member channel). Every change is broadcast as a scope on
//! [`SeekHolds::subscribe`]; each socket sends `seek_hold` when it is its own scope.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use serde_json::{json, Value};
use tokio::{sync::broadcast, time::Instant};
use uuid::Uuid;

use super::{auth::Member, error::ApiResult, ApiState};
use crate::{
    perms::Action,
    player::{state::epoch_ms, PlayerEvent, PlayerHandle},
    store::SessionKey,
};

/// A hold ends at most this long after it started.
pub const HOLD_MAX: Duration = Duration::from_secs(5);
/// After a hold timed out, its holder waits this long before taking it again (others go first).
pub const REACQUIRE_COOLDOWN: Duration = Duration::from_secs(1);

/// What a hold locks: one player channel, or every member channel of a jam.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HoldScope {
    Channel(SessionKey),
    Jam(Arc<str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub id: u64,
    pub user_id: u64,
    /// Display name shown to the others.
    pub name: String,
    /// The socket that asked (closing it releases the hold).
    pub socket: u64,
    /// Current track when it was granted: another one ends the hold.
    pub track: Option<Uuid>,
    pub until: Instant,
    /// `until` as epoch ms, for panels.
    pub until_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquire {
    Granted(Hold),
    /// The user already holds it (kept as it is: the 5 s count from its start).
    Kept(Hold),
    Busy(Hold),
    /// The user's last hold timed out a moment ago.
    Cooldown,
}

/// The hold a socket asks for.
pub struct HoldRequest<'a> {
    pub user_id: u64,
    pub name: &'a str,
    pub socket: u64,
    pub track: Option<Uuid>,
}

#[derive(Default)]
struct State {
    holds: HashMap<HoldScope, Hold>,
    cooldowns: HashMap<(HoldScope, u64), Instant>,
}

pub struct SeekHolds {
    max: Duration,
    cooldown: Duration,
    next: AtomicU64,
    state: Mutex<State>,
    events: broadcast::Sender<HoldScope>,
}

impl SeekHolds {
    pub fn new(max: Duration, cooldown: Duration) -> Self {
        Self {
            max,
            cooldown,
            next: AtomicU64::new(1),
            state: Mutex::default(),
            events: broadcast::channel(256).0,
        }
    }

    /// A scope's hold changed (granted, released, expired, cleared).
    pub fn subscribe(&self) -> broadcast::Receiver<HoldScope> {
        self.events.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn notify(&self, scope: &HoldScope) {
        let _ = self.events.send(scope.clone());
    }

    /// Drop a hold whose time is up (the holder then waits the cooldown). True when one was dropped.
    fn expire_locked(&self, st: &mut State, scope: &HoldScope, now: Instant) -> bool {
        st.cooldowns.retain(|_, until| *until > now);
        match st.holds.get(scope) {
            Some(h) if now >= h.until => {
                let user = h.user_id;
                st.holds.remove(scope);
                st.cooldowns.insert((scope.clone(), user), now + self.cooldown);
                true
            }
            _ => false,
        }
    }

    /// First come wins: a free scope is granted to `req`; the holder keeps its hold.
    pub fn acquire(&self, scope: &HoldScope, req: HoldRequest<'_>, now: Instant, now_ms: i64) -> Acquire {
        let mut st = self.lock();
        let expired = self.expire_locked(&mut st, scope, now);
        let outcome = if let Some(h) = st.holds.get(scope) {
            if h.user_id == req.user_id {
                Acquire::Kept(h.clone())
            } else {
                Acquire::Busy(h.clone())
            }
        } else if st.cooldowns.contains_key(&(scope.clone(), req.user_id)) {
            Acquire::Cooldown
        } else {
            let hold = Hold {
                id: self.next.fetch_add(1, Ordering::Relaxed),
                user_id: req.user_id,
                name: req.name.to_owned(),
                socket: req.socket,
                track: req.track,
                until: now + self.max,
                until_ms: now_ms + i64::try_from(self.max.as_millis()).unwrap_or(i64::MAX),
            };
            st.holds.insert(scope.clone(), hold.clone());
            Acquire::Granted(hold)
        };
        drop(st);
        if expired || matches!(outcome, Acquire::Granted(_)) {
            self.notify(scope);
        }
        outcome
    }

    /// The hold of a scope, if any (one whose time is up is dropped here).
    pub fn current(&self, scope: &HoldScope, now: Instant) -> Option<Hold> {
        let mut st = self.lock();
        let expired = self.expire_locked(&mut st, scope, now);
        let hold = st.holds.get(scope).cloned();
        drop(st);
        if expired {
            self.notify(scope);
        }
        hold
    }

    /// Whether hold `id` is still the scope's hold.
    pub fn is_held(&self, scope: &HoldScope, id: u64) -> bool {
        self.lock().holds.get(scope).is_some_and(|h| h.id == id)
    }

    /// The safety timeout: drop hold `id` once its time is up. True when it was dropped.
    pub fn expire(&self, scope: &HoldScope, id: u64, now: Instant) -> bool {
        let mut st = self.lock();
        if !st.holds.get(scope).is_some_and(|h| h.id == id) {
            return false;
        }
        let expired = self.expire_locked(&mut st, scope, now);
        drop(st);
        if expired {
            self.notify(scope);
        }
        expired
    }

    /// End hold `id` now (track change, player gone). No cooldown.
    pub fn clear(&self, scope: &HoldScope, id: u64) -> bool {
        let mut st = self.lock();
        let removed = st.holds.get(scope).is_some_and(|h| h.id == id);
        if removed {
            st.holds.remove(scope);
        }
        drop(st);
        if removed {
            self.notify(scope);
        }
        removed
    }

    /// Release every hold of a socket (`seek_release`, the socket closed or moved channels).
    pub fn release_socket(&self, socket: u64) {
        let mut st = self.lock();
        let scopes: Vec<HoldScope> = st
            .holds
            .iter()
            .filter(|(_, h)| h.socket == socket)
            .map(|(s, _)| s.clone())
            .collect();
        for s in &scopes {
            st.holds.remove(s);
        }
        drop(st);
        for s in &scopes {
            self.notify(s);
        }
    }

    /// A seek by `user`: refused (with the hold) while someone else holds the bar. The holder's own
    /// seek ends its hold.
    pub fn check_seek(&self, scope: &HoldScope, user: u64, now: Instant) -> Result<(), Hold> {
        let mut st = self.lock();
        let expired = self.expire_locked(&mut st, scope, now);
        let result = match st.holds.get(scope) {
            Some(h) if h.user_id != user => Err(h.clone()),
            Some(_) => {
                st.holds.remove(scope);
                Ok(true)
            }
            None => Ok(false),
        };
        drop(st);
        if expired || result == Ok(true) {
            self.notify(scope);
        }
        result.map(|_| ())
    }
}

impl ApiState {
    /// The hold scope of a player channel: its jam when it is in one.
    pub fn hold_scope(&self, guild_id: u64, channel: u64) -> HoldScope {
        let key = SessionKey {
            guild_id,
            voice_channel_id: channel,
        };
        match self.app.players.jams().of(key) {
            Some(j) => HoldScope::Jam(j.id.clone()),
            None => HoldScope::Channel(key),
        }
    }

    /// `seek_hold` JSON for a channel (see [`hold_json`]).
    pub fn hold_json_of(&self, guild_id: u64, channel: u64) -> Value {
        let hold = self.seek_holds.current(&self.hold_scope(guild_id, channel), Instant::now());
        hold_json(hold.as_ref())
    }

    /// A `seek_hold` message of `socket`: the control rule first (DJ mode, the jam's edit rule),
    /// then first come wins. A new hold gets its watcher (time limit, track change, player gone).
    pub async fn try_hold(&self, m: &Member, channel: u64, socket: u64) -> ApiResult<Acquire> {
        let player = self.allowed(m, channel, |_| Action::Control).await?.player;
        let scope = self.hold_scope(m.guild_id, channel);
        let track = player.snapshot().current.as_ref().map(|e| e.uid);
        let req = HoldRequest {
            user_id: m.user_id(),
            name: m.session.display_name(),
            socket,
            track,
        };
        let outcome = self.seek_holds.acquire(&scope, req, Instant::now(), epoch_ms());
        if let Acquire::Granted(hold) = &outcome {
            self.watch_hold(scope, hold.clone(), player);
        }
        Ok(outcome)
    }

    /// Ends a hold at its time limit, or earlier when the current track changes or the player
    /// stops. Lives at most [`HOLD_MAX`].
    fn watch_hold(&self, scope: HoldScope, hold: Hold, player: PlayerHandle) {
        let st = self.clone();
        tokio::spawn(async move {
            let mut events = player.subscribe();
            // The track may have changed between the grant and the subscription.
            if !same_track(hold.track, player.snapshot().current.as_ref().map(|e| e.uid)) {
                st.seek_holds.clear(&scope, hold.id);
                return;
            }
            let deadline = tokio::time::sleep_until(hold.until);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => {
                        st.seek_holds.expire(&scope, hold.id, Instant::now());
                        return;
                    }
                    ev = events.recv() => {
                        let ends = match ev {
                            Ok(PlayerEvent::State(snap)) => {
                                !same_track(hold.track, snap.current.as_ref().map(|e| e.uid))
                            }
                            Ok(PlayerEvent::Position { .. }) => false,
                            Ok(PlayerEvent::Closed) | Err(broadcast::error::RecvError::Closed) => true,
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                !same_track(hold.track, player.snapshot().current.as_ref().map(|e| e.uid))
                            }
                        };
                        if ends {
                            st.seek_holds.clear(&scope, hold.id);
                            return;
                        }
                        if !st.seek_holds.is_held(&scope, hold.id) {
                            return; // released or used by a seek
                        }
                    }
                }
            }
        });
    }
}

/// `{ holder: { id, name } | null, until: epoch ms | null }`, the `seekHold` of a player state and
/// the body of the `seek_hold` socket message.
pub fn hold_json(hold: Option<&Hold>) -> Value {
    match hold {
        Some(h) => json!({
            "holder": { "id": h.user_id.to_string(), "name": h.name },
            "until": h.until_ms,
        }),
        None => json!({ "holder": null, "until": null }),
    }
}

/// Whether a hold granted on `track` still applies to a player whose current track is `now`.
pub fn same_track(track: Option<Uuid>, now: Option<Uuid>) -> bool {
    track == now
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANA: u64 = 1;
    const BOB: u64 = 2;

    fn scope() -> HoldScope {
        HoldScope::Channel(SessionKey {
            guild_id: 1,
            voice_channel_id: 100,
        })
    }

    fn req(user: u64, socket: u64) -> HoldRequest<'static> {
        HoldRequest {
            user_id: user,
            name: if user == ANA { "Ana" } else { "Bob" },
            socket,
            track: None,
        }
    }

    fn holds() -> SeekHolds {
        SeekHolds::new(HOLD_MAX, REACQUIRE_COOLDOWN)
    }

    #[test]
    fn first_come_wins_and_the_holder_keeps_it() {
        let h = holds();
        let mut rx = h.subscribe();
        let t0 = Instant::now();
        let Acquire::Granted(a) = h.acquire(&scope(), req(ANA, 10), t0, 1_000) else {
            panic!("free scope")
        };
        assert_eq!(a.until, t0 + HOLD_MAX);
        assert_eq!(a.until_ms, 6_000);
        assert_eq!(rx.try_recv().unwrap(), scope());
        // Someone else is refused; the holder asking again keeps the same hold (not extended).
        assert!(matches!(h.acquire(&scope(), req(BOB, 20), t0, 1_000), Acquire::Busy(b) if b.user_id == ANA));
        let later = t0 + Duration::from_secs(3);
        assert_eq!(h.acquire(&scope(), req(ANA, 11), later, 4_000), Acquire::Kept(a.clone()));
        assert!(rx.try_recv().is_err(), "nothing changed");
        // Another scope (another channel, or a jam) is separate.
        let other = HoldScope::Jam("jamid".into());
        assert!(matches!(h.acquire(&other, req(BOB, 20), t0, 1_000), Acquire::Granted(_)));
        assert_eq!(hold_json(Some(&a)), json!({ "holder": { "id": "1", "name": "Ana" }, "until": 6_000 }));
        assert_eq!(hold_json(None), json!({ "holder": null, "until": null }));
    }

    #[test]
    fn a_hold_lasts_at_most_five_seconds_then_the_holder_waits() {
        let h = holds();
        let t0 = Instant::now();
        let Acquire::Granted(a) = h.acquire(&scope(), req(ANA, 10), t0, 0) else {
            panic!()
        };
        let mut rx = h.subscribe();
        // Not yet.
        assert!(!h.expire(&scope(), a.id, t0 + Duration::from_millis(4_999)));
        assert!(h.current(&scope(), t0 + Duration::from_millis(4_999)).is_some());
        // At 5 s the timer drops it and tells the sockets.
        let t5 = t0 + HOLD_MAX;
        assert!(h.expire(&scope(), a.id, t5));
        assert_eq!(rx.try_recv().unwrap(), scope());
        assert!(h.current(&scope(), t5).is_none());
        assert!(!h.expire(&scope(), a.id, t5), "once");
        // The holder can't take it back right away; others can.
        assert_eq!(h.acquire(&scope(), req(ANA, 10), t5 + Duration::from_millis(500), 0), Acquire::Cooldown);
        assert!(matches!(
            h.acquire(&scope(), req(BOB, 20), t5 + Duration::from_millis(500), 0),
            Acquire::Granted(_)
        ));
        h.release_socket(20);
        // After the cooldown the first holder may again.
        assert!(matches!(
            h.acquire(&scope(), req(ANA, 10), t5 + REACQUIRE_COOLDOWN, 0),
            Acquire::Granted(_)
        ));
    }

    #[test]
    fn a_late_timer_still_counts_from_the_start() {
        // Without the timer firing (busy runtime), an expired hold never blocks anyone.
        let h = holds();
        let t0 = Instant::now();
        h.acquire(&scope(), req(ANA, 10), t0, 0);
        let mut rx = h.subscribe();
        let late = t0 + Duration::from_secs(6);
        assert!(h.check_seek(&scope(), BOB, late).is_ok());
        assert_eq!(rx.try_recv().unwrap(), scope(), "the lazy expiry is broadcast");
        assert_eq!(h.acquire(&scope(), req(ANA, 10), late, 0), Acquire::Cooldown);
    }

    #[test]
    fn seeks_of_others_are_refused_and_the_holders_seek_releases() {
        let h = holds();
        let t0 = Instant::now();
        // Nobody holds it: anyone seeks.
        assert!(h.check_seek(&scope(), BOB, t0).is_ok());
        h.acquire(&scope(), req(ANA, 10), t0, 0);
        let mut rx = h.subscribe();
        let refused = h.check_seek(&scope(), BOB, t0).unwrap_err();
        assert_eq!(refused.user_id, ANA);
        assert!(rx.try_recv().is_err());
        // The holder's seek on release goes through and frees the bar, with no cooldown.
        assert!(h.check_seek(&scope(), ANA, t0).is_ok());
        assert_eq!(rx.try_recv().unwrap(), scope());
        assert!(h.current(&scope(), t0).is_none());
        assert!(h.check_seek(&scope(), BOB, t0).is_ok());
        assert!(matches!(h.acquire(&scope(), req(ANA, 10), t0, 0), Acquire::Granted(_)));
    }

    #[test]
    fn closing_the_socket_or_a_new_track_clears_it() {
        let h = holds();
        let t0 = Instant::now();
        let jam = HoldScope::Jam("j".into());
        h.acquire(&scope(), req(ANA, 10), t0, 0);
        h.acquire(&jam, req(ANA, 10), t0, 0);
        h.acquire(&HoldScope::Jam("k".into()), req(BOB, 20), t0, 0);
        let mut rx = h.subscribe();
        h.release_socket(10);
        let mut changed = [rx.try_recv().unwrap(), rx.try_recv().unwrap()];
        changed.sort_by_key(|s| format!("{s:?}"));
        assert!(changed.contains(&scope()) && changed.contains(&jam));
        assert!(h.current(&scope(), t0).is_none());
        assert!(h.current(&HoldScope::Jam("k".into()), t0).is_some(), "other sockets keep theirs");
        // Track change / player gone: cleared, no cooldown; a stale id clears nothing.
        let Acquire::Granted(b) = h.acquire(&scope(), req(ANA, 11), t0, 0) else {
            panic!()
        };
        assert!(!h.clear(&scope(), b.id + 100));
        assert!(h.is_held(&scope(), b.id));
        assert!(h.clear(&scope(), b.id));
        assert!(!h.is_held(&scope(), b.id));
        assert!(matches!(h.acquire(&scope(), req(ANA, 11), t0, 0), Acquire::Granted(_)));
        let t = Uuid::new_v4();
        assert!(same_track(Some(t), Some(t)));
        assert!(!same_track(Some(t), Some(Uuid::new_v4())));
        assert!(!same_track(Some(t), None));
    }
}
