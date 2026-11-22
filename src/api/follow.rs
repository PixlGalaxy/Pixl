//! The voice channel the panel follows.
//!
//! The panel always shows the player of the voice channel the user is connected to right now, wherever
//! the Activity was opened from (a text channel, another voice channel, a message button). Not in a
//! voice channel: nothing is shown. Every player, queue, radio and lyrics route only accepts that
//! channel, so nobody sees or controls another channel's player from the panel (managers included).

use axum::{extract::State, Json};
use serde::Serialize;

use super::{
    auth::{ApiChannel, Member},
    error::{snowflake, ApiError, ApiResult},
    ApiState,
};

#[derive(Serialize)]
pub struct ChannelResponse {
    channel: Option<ApiChannel>,
}

impl ApiState {
    /// Voice channel the panel of `user` follows: their current one in the guild, if any.
    pub fn followed_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
        self.discord.user_voice_channel(guild_id, user_id)
    }

    /// `{ id, name }` of a channel of the guild.
    pub fn channel_ref(&self, guild_id: u64, channel_id: Option<u64>) -> Option<ApiChannel> {
        channel_id.map(|id| ApiChannel {
            id: id.to_string(),
            name: self.discord.channel_name(guild_id, id).unwrap_or_default(),
        })
    }

    /// The `channelId` of a panel request, accepted only when it is the user's current voice channel.
    pub fn own_channel(&self, m: &Member, raw: &str) -> ApiResult<u64> {
        let channel = snowflake(raw)?;
        if self.followed_channel(m.guild_id, m.user_id()) != Some(channel) {
            return Err(ApiError::not_in_channel());
        }
        Ok(channel)
    }
}

/// `GET /api/me/channel`: the followed channel (polling fallback of the socket's `channel` message).
pub async fn me_channel(State(st): State<ApiState>, m: Member) -> Json<ChannelResponse> {
    let channel = st.followed_channel(m.guild_id, m.user_id());
    Json(ChannelResponse {
        channel: st.channel_ref(m.guild_id, channel),
    })
}
