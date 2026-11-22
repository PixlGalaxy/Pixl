//! Discord side of the AI radio: `/ai`, the "Start radio" / "Stop radio" / "Cancel" buttons and
//! the "Radio is picking songs" card that follows the job. Every rule lives in `player::radio`;
//! this module only turns its results into messages.
//!
//! `/ai` takes its choices as slash command options (prompt, mix, blend). The approved design is
//! a modal (Label + Text Input, Radio Group, Checkbox); [`radio_modal`] and [`parse_radio_modal`]
//! build and read it, but serenity 0.12 parses modal submits into action rows and drops the values
//! of Label children, so the modal is not wired until the raw submit payload can be read
//! ([`USE_MODAL`]).

use std::{sync::Arc, time::Duration};

use serde_json::{json, Value};
use serenity::{all::InteractionId, http::Http};
use tokio::{
    sync::watch,
    time::{sleep_until, timeout_at, Instant},
};
use uuid::Uuid;

use super::{
    respond,
    ui::{
        callback,
        cards::{self, RadioPicking, Step},
        defer_callback, Message,
    },
    Lynx,
};
use crate::{
    ai::{AiPhase, RadioError, RadioMix},
    i18n::I18n,
    player::{
        radio::{self, BotUnavailable, RadioApiError, RadioProgress, RadioStartRequest},
        Requester,
    },
};

/// Answer `/ai` with the modal instead of reading command options (see the module docs).
pub const USE_MODAL: bool = false;
pub const MODAL_ID: &str = "ai:modal";
const PROMPT_ID: &str = "prompt";
const MIX_ID: &str = "mix";
const BLEND_ID: &str = "blend";
/// At most one edit of the "picking songs" card per this interval.
pub const EDIT_EVERY: Duration = Duration::from_millis(1500);
/// Interaction tokens live 15 minutes; stop following before that.
const FOLLOW_MAX: Duration = Duration::from_secs(14 * 60);

/// Phase lines of the "picking songs" card, in the order the engine goes through them.
pub const PHASE_LINES: [AiPhase; 6] = [
    AiPhase::Queued,
    AiPhase::ReadingTaste,
    AiPhase::FindingSongs,
    AiPhase::LoadingModel,
    AiPhase::Thinking,
    AiPhase::Resolving,
];

/// What the user asked for in `/ai` (options or modal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioForm {
    pub prompt: Option<String>,
    pub mix: RadioMix,
    pub blend: bool,
}

impl Default for RadioForm {
    /// The "Start radio" button: balanced, blend on, no prompt.
    fn default() -> Self {
        Self {
            prompt: None,
            mix: RadioMix::Balanced,
            blend: true,
        }
    }
}

/// The interaction being answered (built by the router).
pub struct RadioCtx<'a> {
    pub lynx: &'a Lynx,
    pub http: &'a Http,
    pub id: InteractionId,
    pub token: &'a str,
    pub guild_id: u64,
    pub user: Requester,
    pub text_channel: u64,
    pub permissions: Option<u64>,
    pub user_voice: Option<u64>,
}

impl RadioCtx<'_> {
    fn i18n(&self) -> &I18n {
        &self.lynx.i18n
    }

    fn err(&self, key: &str, args: &[(&str, &str)]) -> Message {
        cards::error_key(key, args, self.i18n())
    }
}

// ---------------------------------------------------------------------------
// Modal (built and parsed as raw JSON: serenity has no types for these components)
// ---------------------------------------------------------------------------

/// Interaction callback (type 9) with the "Start a radio" modal: Label + Text Input (prompt,
/// optional), Label + Radio Group (mix, Balanced preselected), Label + Checkbox (blend, on).
pub fn radio_modal(i18n: &I18n) -> Value {
    let t = |k: &str| i18n.t(k, &[]);
    let option = |value: &str, default: bool| json!({ "value": value, "label": t(&format!("bot.radio.mix.{value}")), "default": default });
    json!({
        "type": 9,
        "data": {
            "custom_id": MODAL_ID,
            "title": truncate_chars(&t("bot.radio.modal.title"), 45),
            "components": [
                {
                    "type": 18,
                    "label": truncate_chars(&t("bot.radio.modal.prompt_label"), 45),
                    "description": truncate_chars(&t("bot.radio.modal.prompt_description"), 100),
                    "component": {
                        "type": 4,
                        "custom_id": PROMPT_ID,
                        "style": 1,
                        "required": false,
                        "max_length": 200,
                        "placeholder": truncate_chars(&t("bot.radio.modal.prompt_placeholder"), 100),
                    }
                },
                {
                    "type": 18,
                    "label": truncate_chars(&t("bot.radio.modal.mix_label"), 45),
                    "component": {
                        "type": 21,
                        "custom_id": MIX_ID,
                        "required": true,
                        "options": [option("favorites", false), option("balanced", true), option("discover", false)],
                    }
                },
                {
                    "type": 18,
                    "label": truncate_chars(&t("bot.radio.modal.listeners_label"), 45),
                    "description": truncate_chars(&t("bot.radio.modal.blend"), 100),
                    "component": { "type": 23, "custom_id": BLEND_ID, "default": true }
                }
            ]
        }
    })
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Read the modal submit's raw `data` object. Missing or unknown values fall back to the
/// defaults (no prompt, balanced, blend on). None when it is not the radio modal.
#[allow(dead_code)] // Wired once the raw modal submit is available (see the module docs).
pub fn parse_radio_modal(data: &Value) -> Option<RadioForm> {
    if data.get("custom_id").and_then(Value::as_str) != Some(MODAL_ID) {
        return None;
    }
    let mut form = RadioForm::default();
    let rows = data.get("components").and_then(Value::as_array)?;
    // Label children sit in `component`; older action rows keep them in `components`.
    let children = rows.iter().flat_map(|row| {
        row.get("component")
            .into_iter()
            .chain(row.get("components").and_then(Value::as_array).into_iter().flatten())
    });
    for c in children {
        let value = c.get("value");
        match c.get("custom_id").and_then(Value::as_str) {
            Some(PROMPT_ID) => form.prompt = radio::clean_prompt(value.and_then(Value::as_str)),
            Some(MIX_ID) => {
                if let Some(m) = value.and_then(Value::as_str).and_then(RadioMix::parse) {
                    form.mix = m;
                }
            }
            Some(BLEND_ID) => {
                if let Some(b) = value.and_then(Value::as_bool) {
                    form.blend = b;
                }
            }
            _ => {}
        }
    }
    Some(form)
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

/// Deliver the final answer of a publicly deferred interaction.
async fn finish(http: &Http, token: &str, msg: &Message) {
    if msg.ephemeral {
        respond::delete_original(http, token).await;
        respond::followup(http, token, &msg.to_json()).await;
    } else {
        respond::edit_original(http, token, &msg.to_json()).await;
    }
}

fn start_error(ctx: &RadioCtx, vc: u64, e: &RadioApiError) -> Message {
    let i18n = ctx.i18n();
    match e {
        RadioApiError::HistoryDisabled => cards::history_off(vc, i18n),
        RadioApiError::NotInChannel => ctx.err("not_in_voice", &[]),
        RadioApiError::NoPermission => ctx.err("radio_no_permission", &[]),
        RadioApiError::QueueFull { max } => ctx.err("queue_full", &[("max", &max.to_string())]),
        RadioApiError::BotUnavailable(BotUnavailable::Busy { busy_channel }) => {
            let invite = ctx.lynx.bots.next_node_invite(ctx.guild_id);
            cards::all_busy(*busy_channel, vc, invite.as_ref().map(|(n, u)| (*n, u.as_str())), i18n)
        }
        RadioApiError::BotUnavailable(BotUnavailable::CannotJoin) => {
            ctx.err("cannot_connect", &[("channel", &format!("<#{vc}>"))])
        }
        RadioApiError::BotUnavailable(BotUnavailable::JoinFailed) => ctx.err("join_failed", &[]),
        RadioApiError::BotUnavailable(BotUnavailable::ShuttingDown) => ctx.err("restarting", &[]),
        RadioApiError::ChannelNotAllowed { allowed } => {
            ctx.err("voice_channel", &[("channels", &super::restrict::mentions(allowed))])
        }
        RadioApiError::NotFound | RadioApiError::Unavailable => ctx.err("radio_unavailable", &[]),
        RadioApiError::InJam => ctx.err("radio_in_jam", &[]),
    }
}

/// `/ai` (after reading the options or the modal) and the "Start radio" button: start in the
/// user's voice channel and follow the job on a public card.
pub async fn start(ctx: &RadioCtx<'_>, form: RadioForm) {
    let lynx = ctx.lynx;
    let respond_now = |msg: Message| async move {
        respond::respond(ctx.http, ctx.id, ctx.token, &msg.to_callback(callback::MESSAGE)).await;
    };
    if !lynx.config.ai_enabled {
        return respond_now(ctx.err("radio_unavailable", &[])).await;
    }
    let Some(vc) = ctx.user_voice else {
        return respond_now(ctx.err("not_in_voice", &[])).await;
    };
    // Fast answer for history off (no deferral needed).
    if lynx.store.history_enabled(ctx.user.id).await.ok() == Some(false) {
        return respond_now(cards::history_off(vc, ctx.i18n())).await;
    }
    respond::respond(ctx.http, ctx.id, ctx.token, &defer_callback(false)).await;
    let prompt = radio::clean_prompt(form.prompt.as_deref());
    let req = RadioStartRequest {
        guild_id: ctx.guild_id,
        voice_channel_id: vc,
        user: ctx.user.clone(),
        interaction_permissions: ctx.permissions,
        text_channel_id: Some(ctx.text_channel),
        prompt: prompt.clone(),
        mix: form.mix,
        blend: form.blend,
    };
    let started = match radio::start_radio(&lynx.players, req).await {
        Ok(s) => s,
        Err(e) => return finish(ctx.http, ctx.token, &start_error(ctx, vc, &e)).await,
    };
    let progress = started.progress.borrow().clone();
    let job = started.job_id.simple().to_string();
    let mut hw = 0;
    let msg = picking_card(&progress, &mut hw, vc, prompt.as_deref(), &job, ctx.i18n());
    respond::edit_original(ctx.http, ctx.token, &msg.to_json()).await;
    let follow = Follow {
        http: lynx.bots.main().http.clone(),
        i18n: lynx.i18n.clone(),
        token: ctx.token.to_owned(),
        voice_channel_id: vc,
        prompt,
        job,
        high_water: hw,
    };
    tokio::spawn(follow.run(started.progress));
}

/// Index of a phase in [`PHASE_LINES`].
fn line_index(p: AiPhase) -> Option<usize> {
    PHASE_LINES.iter().position(|x| *x == p)
}

/// Badges of the phase lines: the current phase is Working, every line before the furthest
/// phase reached (`high_water`, a line index) is Done, the rest is Next.
pub fn steps(current: AiPhase, high_water: usize) -> Vec<(AiPhase, Step)> {
    PHASE_LINES
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let step = if *p == current {
                Step::Working
            } else if i < high_water {
                Step::Done
            } else {
                Step::Next
            };
            (*p, step)
        })
        .collect()
}

fn picking_card(
    p: &RadioProgress,
    high_water: &mut usize,
    vc: u64,
    prompt: Option<&str>,
    job: &str,
    i18n: &I18n,
) -> Message {
    if let Some(i) = line_index(p.phase) {
        *high_water = (*high_water).max(i);
    }
    let steps = steps(p.phase, *high_water);
    cards::radio_picking(
        &RadioPicking {
            voice_channel_id: vc,
            prompt,
            first_playing: p.first_queued,
            steps: &steps,
            job,
        },
        i18n,
    )
}

/// The card shown once the start job ended.
pub fn final_card(p: &RadioProgress, vc: u64, i18n: &I18n) -> Message {
    match p.phase {
        AiPhase::Ready => cards::radio_on(p.added, vc, i18n),
        AiPhase::Failed if p.added > 0 => cards::radio_on(p.added, vc, i18n),
        AiPhase::Failed => cards::radio_failed(p.error == Some(RadioError::NoCandidates), i18n),
        _ => cards::radio_cancelled(vc, i18n),
    }
}

/// How long to wait before the next edit (at most one per [`EDIT_EVERY`]).
pub fn edit_delay(last_edit: Instant, now: Instant) -> Duration {
    (last_edit + EDIT_EVERY).saturating_duration_since(now)
}

/// Edits the "picking songs" card as the job advances (throttled), then the final card.
struct Follow {
    http: Arc<Http>,
    i18n: Arc<I18n>,
    token: String,
    voice_channel_id: u64,
    prompt: Option<String>,
    job: String,
    high_water: usize,
}

impl Follow {
    async fn run(mut self, mut rx: watch::Receiver<RadioProgress>) {
        let deadline = Instant::now() + FOLLOW_MAX;
        let mut last_edit = Instant::now();
        let (mut shown, done) = {
            let p = rx.borrow_and_update();
            ((p.phase, p.first_queued), p.phase.is_final())
        };
        let mut first = true;
        loop {
            // The job may have ended before this task started: no change would come.
            let closed = if first && done {
                false
            } else {
                match timeout_at(deadline, rx.changed()).await {
                    Ok(Ok(())) => false,
                    Ok(Err(_)) => true,
                    Err(_) => return,
                }
            };
            first = false;
            let wait = edit_delay(last_edit, Instant::now());
            if !wait.is_zero() {
                sleep_until(Instant::now() + wait).await;
            }
            let p = rx.borrow_and_update().clone();
            if p.phase.is_final() || closed {
                let msg = if p.phase.is_final() {
                    final_card(&p, self.voice_channel_id, &self.i18n)
                } else {
                    cards::radio_cancelled(self.voice_channel_id, &self.i18n)
                };
                respond::edit_original(&self.http, &self.token, &msg.to_json()).await;
                return;
            }
            if (p.phase, p.first_queued) == shown {
                continue;
            }
            shown = (p.phase, p.first_queued);
            let msg = picking_card(
                &p,
                &mut self.high_water,
                self.voice_channel_id,
                self.prompt.as_deref(),
                &self.job,
                &self.i18n,
            );
            respond::edit_original(&self.http, &self.token, &msg.to_json()).await;
            last_edit = Instant::now();
        }
    }
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

fn control_error(ctx: &RadioCtx, e: &RadioApiError, not_found: &str) -> Message {
    match e {
        RadioApiError::NoPermission => ctx.err("radio_no_permission", &[]),
        RadioApiError::NotInChannel => ctx.err("not_in_channel", &[]),
        RadioApiError::Unavailable => ctx.err("radio_unavailable", &[]),
        _ => ctx.err(not_found, &[]),
    }
}

async fn reply_ephemeral(ctx: &RadioCtx<'_>, msg: Message) {
    respond::respond(
        ctx.http,
        ctx.id,
        ctx.token,
        &msg.ephemeral(true).to_callback(callback::MESSAGE),
    )
    .await;
}

/// "Stop radio": the radio ends, the card says who stopped it.
pub async fn stop_button(ctx: &RadioCtx<'_>, vc: u64) {
    match radio::stop_radio(&ctx.lynx.players, ctx.guild_id, vc, ctx.user.id, ctx.permissions).await {
        Ok(()) => {
            let msg = cards::radio_stopped(ctx.user.id, ctx.i18n());
            respond::respond(ctx.http, ctx.id, ctx.token, &msg.to_callback(callback::UPDATE_MESSAGE)).await;
        }
        Err(e) => reply_ephemeral(ctx, control_error(ctx, &e, "radio_not_active")).await,
    }
}

/// "Cancel" on the picking card: the follow task turns the card into "Radio cancelled".
pub async fn cancel_button(ctx: &RadioCtx<'_>, vc: u64, job: &str) {
    let Ok(job) = Uuid::parse_str(job) else {
        return reply_ephemeral(ctx, ctx.err("radio_nothing_to_cancel", &[])).await;
    };
    let r = radio::cancel_radio_job(
        &ctx.lynx.players,
        ctx.guild_id,
        vc,
        ctx.user.id,
        ctx.permissions,
        Some(job),
    )
    .await;
    match r {
        Ok(()) => {
            respond::respond(ctx.http, ctx.id, ctx.token, &json!({ "type": 6 })).await;
        }
        Err(e) => reply_ephemeral(ctx, control_error(ctx, &e, "radio_nothing_to_cancel")).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i18n() -> I18n {
        I18n::load("en").unwrap()
    }

    #[test]
    fn modal_json_shape() {
        let v = radio_modal(&i18n());
        assert_eq!(v["type"], 9);
        let d = &v["data"];
        assert_eq!(d["custom_id"], MODAL_ID);
        assert_eq!(d["title"], "Start a radio");
        let c = d["components"].as_array().unwrap();
        assert_eq!(c.len(), 3);
        assert!(c
            .iter()
            .all(|l| l["type"] == 18 && l["label"].as_str().unwrap().len() <= 45));
        assert_eq!(c[0]["label"], "What do you want to hear?");
        assert_eq!(
            c[0]["component"],
            json!({
                "type": 4, "custom_id": "prompt", "style": 1, "required": false, "max_length": 200,
                "placeholder": "something calm to study"
            })
        );
        assert_eq!(c[1]["label"], "Mix");
        assert_eq!(c[1]["component"]["type"], 21);
        assert_eq!(
            c[1]["component"]["options"],
            json!([
                { "value": "favorites", "label": "Mostly my favorites", "default": false },
                { "value": "balanced", "label": "Balanced", "default": true },
                { "value": "discover", "label": "Discover new music", "default": false }
            ])
        );
        assert_eq!(c[2]["description"], "Blend the taste of everyone in the channel");
        assert_eq!(
            c[2]["component"],
            json!({ "type": 23, "custom_id": "blend", "default": true })
        );
        // Spanish fits the limits too.
        let es = radio_modal(&I18n::load("es").unwrap());
        assert!(es["data"]["title"].as_str().unwrap().chars().count() <= 45);
    }

    #[test]
    fn modal_submit_parsing() {
        // Shape of a MODAL_SUBMIT `data` object (Label children under `component`).
        let data = json!({
            "custom_id": "ai:modal",
            "components": [
                { "type": 18, "id": 1, "component": { "type": 4, "id": 2, "custom_id": "prompt", "value": "  something calm to study " } },
                { "type": 18, "id": 3, "component": { "type": 21, "id": 4, "custom_id": "mix", "value": "discover" } },
                { "type": 18, "id": 5, "component": { "type": 23, "id": 6, "custom_id": "blend", "value": false } }
            ]
        });
        assert_eq!(
            parse_radio_modal(&data),
            Some(RadioForm {
                prompt: Some("something calm to study".into()),
                mix: RadioMix::Discover,
                blend: false,
            })
        );
        // Empty prompt, no radio selection, unknown values: defaults.
        let data = json!({
            "custom_id": "ai:modal",
            "components": [
                { "type": 18, "component": { "type": 4, "custom_id": "prompt", "value": "" } },
                { "type": 18, "component": { "type": 21, "custom_id": "mix", "value": null } },
                { "type": 18, "component": { "type": 23, "custom_id": "blend", "value": true } }
            ]
        });
        assert_eq!(parse_radio_modal(&data), Some(RadioForm::default()));
        let data = json!({ "custom_id": "ai:modal", "components": [
            { "type": 18, "component": { "type": 21, "custom_id": "mix", "value": "loud" } }
        ]});
        assert_eq!(parse_radio_modal(&data).unwrap().mix, RadioMix::Balanced);
        // Legacy action row with a text input.
        let data = json!({ "custom_id": "ai:modal", "components": [
            { "type": 1, "components": [{ "type": 4, "custom_id": "prompt", "value": "jazz" }] }
        ]});
        assert_eq!(parse_radio_modal(&data).unwrap().prompt.as_deref(), Some("jazz"));
        assert_eq!(
            parse_radio_modal(&json!({ "custom_id": "other", "components": [] })),
            None
        );
    }

    #[test]
    fn phase_steps() {
        let s = steps(AiPhase::Queued, 0);
        assert_eq!(s[0], (AiPhase::Queued, Step::Working));
        assert!(s[1..].iter().all(|(_, st)| *st == Step::Next));
        let s = steps(AiPhase::Thinking, 4);
        assert_eq!(
            s.iter().map(|(_, st)| *st).collect::<Vec<_>>(),
            vec![
                Step::Done,
                Step::Done,
                Step::Done,
                Step::Done,
                Step::Working,
                Step::Next
            ]
        );
        // Back to finding songs after the model read the prompt: later lines stay done.
        let s = steps(AiPhase::FindingSongs, 4);
        assert_eq!(s[2].1, Step::Working);
        assert_eq!(s[3].1, Step::Done);
        assert_eq!(s[5].1, Step::Next);
    }

    fn progress(phase: AiPhase, added: usize, error: Option<RadioError>) -> RadioProgress {
        RadioProgress {
            job_id: Uuid::nil(),
            phase,
            added,
            first_queued: added > 0,
            truncated: false,
            error,
        }
    }

    fn text(m: &Message) -> String {
        m.to_json().to_string()
    }

    #[test]
    fn cards_for_each_outcome() {
        let i = i18n();
        let mut hw = 0;
        let card = picking_card(
            &progress(AiPhase::ReadingTaste, 1, None),
            &mut hw,
            5,
            Some("calm"),
            "abc",
            &i,
        );
        assert_eq!(hw, 1);
        let t = text(&card);
        assert!(
            t.contains("Radio is picking songs") && t.contains("\\\"calm\\\""),
            "{t}"
        );
        assert!(t.contains("The first song is already playing."));
        assert!(t.contains("Done - Waiting in line") && t.contains("**Working** - **Reading your taste**"));
        assert!(t.contains("\"custom_id\":\"rc:5:abc\"") && t.contains("\"style\":4"));

        let t = text(&final_card(&progress(AiPhase::Ready, 20, None), 5, &i));
        assert!(t.contains("Radio is on") && t.contains("Added 20 songs") && t.contains("rs:5"));
        let t = text(&final_card(
            &progress(AiPhase::Failed, 0, Some(RadioError::NoCandidates)),
            5,
            &i,
        ));
        assert!(t.contains("could not find songs"), "{t}");
        let t = text(&final_card(
            &progress(AiPhase::Failed, 1, Some(RadioError::Internal("x".into()))),
            5,
            &i,
        ));
        assert!(t.contains("Added 1 song to"), "the early pick plays: the radio is on");
        let t = text(&final_card(&progress(AiPhase::Cancelled, 1, None), 5, &i));
        assert!(t.contains("Radio cancelled."));
        let t = text(&cards::radio_stopped(9, &i));
        assert!(t.contains("<@9> turned the radio off"));
        for m in [card, cards::radio_on(3, 5, &i)] {
            assert!(!m.ephemeral);
            assert!(m.text_len() < 1000);
        }
    }

    #[test]
    fn edits_are_throttled() {
        let t0 = Instant::now();
        assert_eq!(edit_delay(t0, t0), EDIT_EVERY);
        assert_eq!(
            edit_delay(t0, t0 + Duration::from_millis(500)),
            Duration::from_millis(1000)
        );
        assert!(edit_delay(t0, t0 + Duration::from_secs(2)).is_zero());
    }

    #[test]
    fn form_defaults() {
        assert_eq!(
            RadioForm::default(),
            RadioForm {
                prompt: None,
                mix: RadioMix::Balanced,
                blend: true
            }
        );
    }
}
