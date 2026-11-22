//! One player actor per active voice channel (guild + channel), driven by commands, voice events and the API.

#![allow(dead_code)]

pub mod lifecycle;
pub mod queue;
pub mod state;
pub mod votes;
