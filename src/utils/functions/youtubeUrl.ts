import type { URL } from "node:url";

const youtubeHosts = /(?:^|\.)(?:youtube\.com|youtube-nocookie\.com|youtu\.be)$/u;
const videoIdRegex = /^[\w-]{11}$/u;

export function isYouTubeURL(url: URL): boolean {
    return youtubeHosts.test(url.hostname);
}

// Supports watch?v=, youtu.be/, /shorts/, /live/, /embed/ and /v/ links (incl. music. and m. subdomains).
export function getYouTubeVideoId(url: URL): string | null {
    if (!isYouTubeURL(url)) return null;

    if (url.hostname.endsWith("youtu.be")) {
        const shortId = url.pathname.split("/")[1] ?? "";
        return videoIdRegex.test(shortId) ? shortId : null;
    }

    const videoParam = url.searchParams.get("v");
    if (videoParam !== null && videoIdRegex.test(videoParam)) return videoParam;

    const [, kind, id] = url.pathname.split("/");
    if (["shorts", "live", "embed", "v"].includes(kind ?? "") && videoIdRegex.test(id ?? "")) return id ?? null;

    return null;
}

/**
 * Returns the playlist id, ignoring auto-generated radio/mix lists (RD...), which YouTube does not
 * expose as regular playlists and used to make the whole request fail.
 */
export function getYouTubePlaylistId(url: URL): string | null {
    if (!isYouTubeURL(url)) return null;
    const list = url.searchParams.get("list");
    if (list === null || list.length === 0 || list.startsWith("RD")) return null;
    return list;
}
