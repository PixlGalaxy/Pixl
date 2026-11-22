//! Songbird inputs for a prepared track and the track event bridge back into the player actor.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use songbird::{
    input::{AudioStream, HttpRequest, Input, LiveInput},
    Event, EventContext, EventHandler, TrackEvent,
};
use symphonia::core::io::{MediaSource, ReadOnlySource};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{http_seek::SeekableHttpRequest, Command};
use crate::{
    procs::{ProcOwner, ProcessRegistry, StreamChild},
    sources::{Playable, Sources, StreamInfo},
};

/// Longest a yt-dlp piped stream may run (live streams, very long mixes).
pub const MAX_STREAM_DURATION: Duration = Duration::from_secs(6 * 60 * 60);

/// Direct HTTP read of the stream URL (normal case: songbird passes Opus through untouched).
/// Seekable in both directions when the URL carries its byte length.
pub fn http_input(client: reqwest::Client, stream: &StreamInfo) -> Input {
    let mut headers = HeaderMap::new();
    for (k, v) in &stream.http_headers {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            headers.insert(name, value);
        }
    }
    // With the length known, seeks become bounded range requests (YouTube only answers `bytes=a-b`)
    // instead of reading the stream up to the target, and seeking backward works.
    match content_length(&stream.url) {
        Some(len) => SeekableHttpRequest::new(client, stream.url.clone(), headers, len).into(),
        None => HttpRequest::new_with_headers(client, stream.url.clone(), headers).into(),
    }
}

/// Byte length of a googlevideo URL (`clen` query parameter), when present.
fn content_length(url: &str) -> Option<u64> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let clen = parsed.query_pairs().find(|(k, _)| k == "clen")?.1;
    clen.parse().ok().filter(|n| *n > 0)
}

#[cfg(test)]
mod content_length_tests {
    use super::content_length;

    #[test]
    fn reads_clen() {
        let url = "https://rr1---sn-x.googlevideo.com/videoplayback?expire=1&itag=251&clen=3437753&dur=212.061";
        assert_eq!(content_length(url), Some(3_437_753));
        assert_eq!(content_length("https://example.com/a.opus"), None);
        assert_eq!(content_length("not a url"), None);
    }
}

/// Pipe the page through yt-dlp (fragmented or live streams). The child is registered in the
/// process registry under the track's owner, so `kill_track` ends it; dropping the input kills it.
pub async fn ytdlp_input(
    sources: &Sources,
    procs: &ProcessRegistry,
    playable: &Playable,
    owner: &ProcOwner,
) -> std::io::Result<Input> {
    let dir = sources.stream_dirs().create_for(owner).await?;
    let spec = sources
        .ytdlp()
        .stream_spec(&playable.page_url, owner, &dir, MAX_STREAM_DURATION);
    let child: StreamChild = procs
        .spawn_stream(spec)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let stream = AudioStream {
        input: Box::new(ReadOnlySource::new(child)) as Box<dyn MediaSource>,
    };
    Ok(Input::Live(LiveInput::Raw(stream), None))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSignal {
    Playable,
    End,
    Error,
}

/// Forwards songbird track events of one track (identified by songbird's handle uuid) to the actor.
pub struct TrackNotifier {
    pub tx: mpsc::Sender<Command>,
    pub handle_id: Uuid,
    pub signal: TrackSignal,
}

#[serenity::async_trait]
impl EventHandler for TrackNotifier {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        // songbird fires End before Error for a track that failed: report it as an error so the
        // player shows the failure notice and skips instead of treating it as a normal end.
        let mut signal = self.signal;
        if let EventContext::Track(tracks) = ctx {
            for (state, _) in tracks.iter() {
                if let songbird::tracks::PlayMode::Errored(e) = &state.playing {
                    tracing::warn!(error = %e, handle = %self.handle_id, "track failed");
                    signal = TrackSignal::Error;
                }
            }
        }
        let cmd = Command::Track {
            handle_id: self.handle_id,
            signal,
        };
        if tokio::time::timeout(Duration::from_secs(5), self.tx.send(cmd))
            .await
            .is_err()
        {
            tracing::warn!("player did not take a track event in time");
        }
        None
    }
}

pub fn track_events() -> [(TrackEvent, TrackSignal); 3] {
    [
        (TrackEvent::Playable, TrackSignal::Playable),
        (TrackEvent::End, TrackSignal::End),
        (TrackEvent::Error, TrackSignal::Error),
    ]
}
