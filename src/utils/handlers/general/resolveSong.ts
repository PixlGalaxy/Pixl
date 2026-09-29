import type { VideoCompact } from "youtubei";
import type { Song } from "../../../typings/index.js";
import { youtube } from "../YouTubeUtil.js";

const unwantedTags = ["live", "cover", "remix", "karaoke", "sped up", "slowed", "nightcore", "8d", "instrumental", "reverb"];
const cache = new Map<string, string>();
const maxCacheSize = 5_000;

function normalize(text: string): string {
    return text
        .toLowerCase()
        .normalize("NFKD")
        .replaceAll(/[̀-ͯ]/gu, "")
        .replaceAll(/\((?:feat|ft|with)\.?[^)]*\)/gu, "")
        .replaceAll(/[^\p{L}\p{N}]+/gu, " ")
        .trim();
}

// Scores how likely a YouTube result is the same recording as the Spotify track. Higher is better.
export function scoreVideo(
    video: Pick<VideoCompact, "duration" | "isLive" | "title"> & { channel?: { name: string } },
    target: Pick<Song, "artist" | "duration" | "title">
): number {
    const title = normalize(video.title);
    const channel = normalize(video.channel?.name ?? "");
    const wanted = normalize(target.title);
    const artists = (target.artist ?? "").split(",").map(artist => normalize(artist)).filter(Boolean);
    let score = 0;

    if (video.isLive) score -= 20;
    if (wanted.length > 0 && title.includes(wanted)) score += 3;
    if (artists.some(artist => title.includes(artist) || channel.includes(artist))) score += 2;
    if ((video.channel?.name ?? "").endsWith(" - Topic")) score += 3;

    if ((video.duration ?? 0) > 0 && target.duration > 0) {
        const diff = Math.abs((video.duration ?? 0) - target.duration);
        if (diff <= 3) score += 4;
        else if (diff <= 10) score += 2;
        else if (diff > 30) score -= 3;
    }

    for (const tag of unwantedTags) {
        if (title.includes(tag) && !wanted.includes(tag)) score -= 3;
    }

    return score;
}

async function searchVideos(query: string): Promise<VideoCompact[]> {
    const result = await youtube.search(query, { type: "video" }).catch(() => null);
    return result?.items ?? [];
}

export async function findYouTubeMatch(song: Pick<Song, "artist" | "duration" | "isrc" | "title">): Promise<VideoCompact | null> {
    const primaryArtist = (song.artist ?? "").split(",")[0]?.trim() ?? "";
    const candidates = await searchVideos(primaryArtist.length > 0 ? `${primaryArtist} - ${song.title}` : song.title);
    const rank = (videos: VideoCompact[]): { video: VideoCompact; score: number } | null =>
        videos
            .map(video => ({ score: scoreVideo(video, song), video }))
            .sort((a, b) => b.score - a.score)[0] ?? null;

    let best = rank(candidates);
    if ((best === null || best.score < 5) && (song.isrc?.length ?? 0) > 0) {
        const byIsrc = rank(await searchVideos(song.isrc ?? ""));
        if (byIsrc && (best === null || byIsrc.score > best.score)) best = byIsrc;
    }

    return best?.video ?? null;
}

/**
 * Makes sure a queued song has something we can stream. Spotify tracks are matched to
 * YouTube lazily, right before they play, so big playlists are queued instantly and in order.
 */
export async function ensurePlayable(song: Song): Promise<string> {
    if (song.source !== "spotify") return song.streamUrl ?? song.url;
    if ((song.streamUrl?.length ?? 0) > 0) return song.streamUrl ?? "";

    const cached = cache.get(song.id);
    if (cached !== undefined) {
        song.streamUrl = cached;
        return cached;
    }

    const match = await findYouTubeMatch(song);
    if (!match) throw new Error(`No playable source found for "${song.artist ?? ""} - ${song.title}"`);

    const url = `https://www.youtube.com/watch?v=${match.id}`;
    if (cache.size >= maxCacheSize) {
        const oldest = cache.keys().next().value;
        if (oldest !== undefined) cache.delete(oldest);
    }
    cache.set(song.id, url);
    song.streamUrl = url;
    if (song.duration <= 0) song.duration = match.duration ?? 0;
    return url;
}
