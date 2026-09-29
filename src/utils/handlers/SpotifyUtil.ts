import { Buffer } from "node:buffer";
import { setTimeout } from "node:timers";
import { URL } from "node:url";
import { clientId, clientSecret } from "../../config/env.js";
import type { Rawon } from "../../structures/Rawon.js";
import type {
    Song,
    SpotifyCollection,
    SpotifyImage,
    SpotifyPage,
    SpotifyPlaylistEntry,
    SpotifyTrack
} from "../../typings/index.js";

export type SpotifyResourceType = "album" | "artist" | "playlist" | "track";

export type SpotifyResolveResult = {
    name?: string;
    tracks: Song[];
};

export class SpotifyError extends Error {
    public constructor(public readonly reason: "notFound" | "privateOrUnavailable" | "spotifyUnavailable") {
        super(`[SpotifyUtil] ${reason}`);
        this.name = "SpotifyError";
    }
}

/**
 * Matches every shape of Spotify link we have seen in the wild:
 * - https://open.spotify.com/playlist/ID
 * - https://open.spotify.com/intl-es/album/ID?si=...
 * - https://open.spotify.com/embed/track/ID
 * - https://open.spotify.com/user/xyz/playlist/ID (legacy)
 * - spotify:track:ID / spotify:user:xyz:playlist:ID
 */
export const spotifyRegex =
    /(?:https?:\/\/(?:open|play)\.spotify\.com\/(?:[\w-]+\/)*?|spotify:(?:user:[^:]+:)?)(?<type>track|playlist|album|artist)[/:](?<id>[\dA-Za-z]{22})/u;

const shortLinkHosts = new Set(["spotify.link", "spotify.app.link"]);
const pageSize = 100;

export function parseSpotifyURL(url: string): { type: SpotifyResourceType; id: string } | null {
    const match = spotifyRegex.exec(url);
    if (!match?.groups) return null;
    return { type: match.groups.type as SpotifyResourceType, id: match.groups.id };
}

export function isSpotifyShortLink(url: string): boolean {
    try {
        return shortLinkHosts.has(new URL(url).hostname);
    } catch {
        return false;
    }
}

function biggestImage(images: SpotifyImage[] | undefined): string {
    return [...(images ?? [])].sort((a, b) => (b.width ?? 0) - (a.width ?? 0))[0]?.url ?? "";
}

// Converts Spotify metadata into a queue entry. The playable source is resolved later.
export function spotifyTrackToSong(track: SpotifyTrack, fallbackThumbnail = ""): Song {
    const artist = track.artists.map(x => x.name).filter(Boolean).join(", ");
    return {
        artist,
        duration: Math.round(track.duration_ms / 1_000),
        id: track.id,
        isrc: track.external_ids?.isrc,
        source: "spotify",
        thumbnail: biggestImage(track.album?.images) || fallbackThumbnail,
        title: track.name,
        url: track.external_urls?.spotify ?? `https://open.spotify.com/track/${track.id}`
    };
}

// Accepts both the classic playlist entry (`track`) and the 2026 one (`item`), and bare tracks.
export function unwrapEntry(entry: SpotifyPlaylistEntry | SpotifyTrack | null | undefined): SpotifyTrack | null {
    if (!entry) return null;
    if ("name" in entry && "artists" in entry) return entry;

    const wrapped = entry;
    if (wrapped.is_local === true) return null;
    const track = wrapped.track ?? wrapped.item ?? null;
    if (!track || track.is_local === true) return null;
    // Podcast episodes and removed tracks cannot be played.
    if (track.type !== undefined && track.type !== "track") return null;
    if ((track.id as string | null | undefined) === null || !Array.isArray(track.artists)) return null;
    return track;
}

export class SpotifyUtil {
    public readonly baseURI = "https://api.spotify.com/v1";
    private token: string | null = null;
    private tokenExpiresAt = 0;
    private tokenPromise: Promise<string> | null = null;

    public constructor(public client: Rawon) {}

    public get hasCredentials(): boolean {
        return clientId.length > 0 && clientSecret.length > 0;
    }

    public async renew(): Promise<void> {
        if (!this.hasCredentials) {
            this.client.logger.warn(
                "[SpotifyUtil] SPOTIFY_CLIENT_ID/SPOTIFY_CLIENT_SECRET are not set, using the public embed fallback (max. 100 tracks per playlist)."
            );
            return;
        }
        try {
            await this.getToken(true);
            const renewIn = Math.max(60_000, this.tokenExpiresAt - Date.now() - 300_000);
            this.client.logger.info(`[SpotifyUtil] Token fetched, renewing in ${(renewIn / 60_000).toFixed(1)} minutes.`);
            setTimeout(async () => this.renew(), renewIn).unref();
        } catch (error) {
            this.client.logger.error("[SpotifyUtil] Failed to fetch Spotify token, retrying in 1 minute: ", error);
            setTimeout(async () => this.renew(), 60_000).unref();
        }
    }

    // Resolves `spotify.link` / `spotify.app.link` share links to a regular open.spotify.com URL.
    public async expandShortLink(url: string): Promise<string> {
        const response = await this.client.request.get(url, { followRedirect: true, throwHttpErrors: false });
        const finalUrl = response.url;
        if (parseSpotifyURL(finalUrl)) return finalUrl;

        const found = /https:\/\/open\.spotify\.com\/(?:[\w-]+\/)*?(?:track|playlist|album|artist)\/[\dA-Za-z]{22}/u.exec(
            response.body
        );
        return found?.[0] ?? finalUrl;
    }

    public async resolve(url: string): Promise<SpotifyResolveResult> {
        const target = isSpotifyShortLink(url) ? await this.expandShortLink(url) : url;
        const parsed = parseSpotifyURL(target);
        if (!parsed) throw new SpotifyError("notFound");

        if (this.hasCredentials) {
            try {
                return await this.resolveWithAPI(parsed.type, parsed.id);
            } catch (error) {
                // Spotify-owned/editorial playlists (37i9dQZF...) return 404 for client-credential apps
                // since late 2024; the embed page still exposes them.
                this.client.logger.warn(
                    `[SpotifyUtil] Web API failed for ${parsed.type}/${parsed.id} (${(error as Error).message}), trying the embed fallback.`
                );
            }
        }

        try {
            return await this.resolveWithEmbed(parsed.type, parsed.id);
        } catch (error) {
            this.client.logger.error(`[SpotifyUtil] Embed fallback failed for ${parsed.type}/${parsed.id}:`, error);
            throw new SpotifyError(this.hasCredentials ? "privateOrUnavailable" : "spotifyUnavailable");
        }
    }

    private async getToken(force = false): Promise<string> {
        if (!force && this.token !== null && Date.now() < this.tokenExpiresAt - 60_000) return this.token;
        if (this.tokenPromise) return this.tokenPromise;

        this.tokenPromise = this.requestToken();
        try {
            return await this.tokenPromise;
        } finally {
            this.tokenPromise = null;
        }
    }

    private async requestToken(): Promise<string> {
        const authString = Buffer.from(`${clientId}:${clientSecret}`).toString("base64");
        let lastError: unknown;
        for (let attempt = 1; attempt <= 3; attempt++) {
            try {
                // eslint-disable-next-line no-await-in-loop
                const data = await this.client.request
                    .post("https://accounts.spotify.com/api/token", {
                        body: "grant_type=client_credentials",
                        headers: {
                            Authorization: `Basic ${authString}`,
                            "Content-Type": "application/x-www-form-urlencoded"
                        }
                    })
                    .json<Record<string, unknown>>();
                const accessToken = data.access_token;
                const expiresIn = data.expires_in;
                if (typeof accessToken !== "string" || typeof expiresIn !== "number") {
                    throw new TypeError("Invalid token response");
                }
                this.token = `Bearer ${accessToken}`;
                this.tokenExpiresAt = Date.now() + expiresIn * 1_000;
                return this.token;
            } catch (error) {
                lastError = error;
                this.client.logger.warn(`[SpotifyUtil] Token attempt ${attempt} failed.`);
            }
        }
        throw lastError;
    }

    private async api<T>(pathOrUrl: string): Promise<T> {
        const url = pathOrUrl.startsWith("http") ? pathOrUrl : `${this.baseURI}${pathOrUrl}`;
        return this.client.request
            .get(url, { headers: { Authorization: await this.getToken() }, retry: { limit: 2 } })
            .json<T>();
    }

    // Walks every page of a paginated Spotify list, keeping the original order.
    private async collectPages(first: SpotifyPage<SpotifyPlaylistEntry | SpotifyTrack> | undefined): Promise<SpotifyTrack[]> {
        const tracks: SpotifyTrack[] = [];
        let page = first;

        while (page) {
            for (const entry of page.items) {
                const track = unwrapEntry(entry);
                if (track) tracks.push(track);
            }
            // eslint-disable-next-line no-await-in-loop
            page = page.next === null ? undefined : await this.api<SpotifyPage<SpotifyPlaylistEntry | SpotifyTrack>>(page.next);
        }

        return tracks;
    }

    private async resolveWithAPI(type: SpotifyResourceType, id: string): Promise<SpotifyResolveResult> {
        switch (type) {
            case "track": {
                const track = await this.api<SpotifyTrack>(`/tracks/${id}`);
                return { tracks: [spotifyTrackToSong(track)] };
            }

            case "album": {
                const album = await this.api<SpotifyCollection>(`/albums/${id}`);
                const cover = biggestImage(album.images);
                const tracks = await this.collectPages(album.tracks ?? album.items);
                return { name: album.name, tracks: tracks.map(track => spotifyTrackToSong(track, cover)) };
            }

            case "playlist": {
                const playlist = await this.api<SpotifyCollection>(`/playlists/${id}`);
                const firstPage =
                    playlist.tracks ??
                    playlist.items ??
                    (await this.api<SpotifyPage<SpotifyPlaylistEntry>>(`/playlists/${id}/tracks?limit=${pageSize}`));
                const tracks = await this.collectPages(firstPage);
                return { name: playlist.name, tracks: tracks.map(track => spotifyTrackToSong(track)) };
            }

            case "artist": {
                const artist = await this.api<{ name: string }>(`/artists/${id}`);
                const top = await this.api<{ tracks: SpotifyTrack[] }>(`/artists/${id}/top-tracks?market=US`);
                return { name: artist.name, tracks: top.tracks.map(track => spotifyTrackToSong(track)) };
            }

            default:
                throw new SpotifyError("notFound");
        }
    }

    /**
     * Reads the public embed player, which works without credentials and for editorial playlists.
     * It only lists the first 100 tracks of a playlist.
     */
    private async resolveWithEmbed(type: SpotifyResourceType, id: string): Promise<SpotifyResolveResult> {
        const html = await this.client.request
            .get(`https://open.spotify.com/embed/${type}/${id}`, {
                headers: { "Accept-Language": "en", "User-Agent": "Mozilla/5.0 (compatible; PixlBot)" }
            })
            .text();

        return parseSpotifyEmbed(html, type, id);
    }
}

type EmbedEntity = {
    name?: string;
    title?: string;
    subtitle?: string;
    id?: string;
    uri?: string;
    duration?: number;
    artists?: { name: string }[];
    coverArt?: { sources?: SpotifyImage[] };
    visualIdentity?: { image?: { url: string; maxWidth?: number }[] };
    trackList?: { uri?: string; title?: string; subtitle?: string; duration?: number; isPlayable?: boolean }[];
};

function findEntity(node: unknown, depth = 0): EmbedEntity | null {
    if (node === null || typeof node !== "object" || depth > 8) return null;
    const record = node as Record<string, unknown>;
    if (typeof record.entity === "object" && record.entity !== null) return record.entity;
    for (const value of Object.values(record)) {
        const found = findEntity(value, depth + 1);
        if (found) return found;
    }
    return null;
}

export function parseSpotifyEmbed(html: string, type: SpotifyResourceType, id: string): SpotifyResolveResult {
    const json = /<script id="__NEXT_DATA__" type="application\/json">(?<data>.+?)<\/script>/su.exec(html)?.groups?.data;
    if (json === undefined) throw new Error("Embed data not found");

    const entity = findEntity(JSON.parse(json));
    if (!entity) throw new Error("Embed entity not found");

    const cover =
        [...(entity.coverArt?.sources ?? [])].sort((a, b) => (b.width ?? 0) - (a.width ?? 0))[0]?.url ??
        [...(entity.visualIdentity?.image ?? [])].sort((a, b) => (b.maxWidth ?? 0) - (a.maxWidth ?? 0))[0]?.url ??
        "";

    if (type === "track" || !Array.isArray(entity.trackList)) {
        const artist = entity.artists?.map(x => x.name).join(", ") ?? entity.subtitle ?? "";
        return {
            tracks: [
                {
                    artist,
                    duration: Math.round((entity.duration ?? 0) / 1_000),
                    id,
                    source: "spotify",
                    thumbnail: cover,
                    title: entity.name ?? entity.title ?? "",
                    url: `https://open.spotify.com/track/${id}`
                }
            ]
        };
    }

    const tracks: Song[] = entity.trackList
        .filter(item => item.isPlayable !== false && (item.uri?.startsWith("spotify:track:") ?? false))
        .map(item => {
            const trackId = item.uri?.split(":").at(-1) ?? "";
            return {
                artist: (item.subtitle ?? "").replaceAll(" ", " "),
                duration: Math.round((item.duration ?? 0) / 1_000),
                id: trackId,
                source: "spotify",
                thumbnail: cover,
                title: item.title ?? "",
                url: `https://open.spotify.com/track/${trackId}`
            };
        });

    return { name: entity.name ?? entity.title, tracks };
}
