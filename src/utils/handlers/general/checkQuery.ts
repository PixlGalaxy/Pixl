import { URL } from "node:url";
import type { QueryData } from "../../../typings/index.js";
import { getYouTubePlaylistId, getYouTubeVideoId, isYouTubeURL } from "../../functions/youtubeUrl.js";
import { parseSpotifyURL } from "../SpotifyUtil.js";

export function checkQuery(string: string): QueryData {
    let url: URL;
    try {
        url = new URL(string.trim());
    } catch {
        return {
            isURL: false,
            sourceType: "query"
        };
    }

    if (url.protocol === "spotify:" || /(?:^|\.)spotify\.(?:com|link|app\.link)$/u.test(url.hostname)) {
        const parsed = parseSpotifyURL(url.toString());
        return {
            isURL: true,
            sourceType: "spotify",
            type: parsed ? (parsed.type === "track" ? "track" : "playlist") : "unknown"
        };
    }

    if (!["http:", "https:"].includes(url.protocol)) {
        return { isURL: false, sourceType: "query" };
    }

    if (/(?:^|\.)(?:soundcloud\.com|snd\.sc)$|soundcloud\.app\.goo\.gl$/u.test(url.hostname)) {
        return {
            isURL: true,
            sourceType: "soundcloud",
            type: url.pathname.includes("/sets/") ? "playlist" : "track"
        };
    }

    if (isYouTubeURL(url)) {
        const playlist = getYouTubePlaylistId(url);
        const video = getYouTubeVideoId(url);
        return {
            isURL: true,
            sourceType: "youtube",
            type: playlist !== null && (video === null || url.pathname.startsWith("/playlist") || url.searchParams.has("list"))
                ? "playlist"
                : video === null
                    ? "unknown"
                    : "track"
        };
    }

    return {
        isURL: true,
        sourceType: "unknown",
        type: "unknown"
    };
}
