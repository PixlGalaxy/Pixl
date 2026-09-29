import { URL } from "node:url";
import type { SoundcloudTrack } from "soundcloud.ts";
import type { VideoCompact } from "youtubei";
import YTI from "youtubei";
import type { Rawon } from "../../../structures/Rawon.js";
import type { SearchTrackResult, Song } from "../../../typings/index.js";
import { getYouTubePlaylistId, getYouTubeVideoId } from "../../functions/youtubeUrl.js";
import { SpotifyError } from "../SpotifyUtil.js";
import { getInfo } from "../YTDLUtil.js";
import { youtube } from "../YouTubeUtil.js";
import { checkQuery } from "./checkQuery.js";

const { Playlist } = YTI;

function bestThumbnail(thumbnails: { url: string; width: number; height: number }[] | undefined): string {
    return [...(thumbnails ?? [])].sort((a, b) => b.height * b.width - a.height * a.width)[0]?.url ?? "";
}

export function youtubeVideoToSong(video: Pick<VideoCompact, "duration" | "id" | "thumbnails" | "title"> & { channel?: { name: string } }): Song {
    return {
        artist: video.channel?.name,
        duration: video.duration ?? 0,
        id: video.id,
        source: "youtube",
        thumbnail: bestThumbnail(video.thumbnails),
        title: video.title,
        url: `https://www.youtube.com/watch?v=${video.id}`
    };
}

function soundcloudTrackToSong(track: SoundcloudTrack): Song {
    return {
        artist: track.publisher_metadata?.artist ?? track.user?.username,
        duration: Math.round(track.full_duration / 1_000),
        id: track.id.toString(),
        source: "soundcloud",
        thumbnail: track.artwork_url,
        title: track.title,
        url: track.permalink_url
    };
}

async function resolveYouTube(url: URL, result: SearchTrackResult): Promise<void> {
    const playlistId = getYouTubePlaylistId(url);
    const videoId = getYouTubeVideoId(url);

    if (playlistId !== null) {
        const playlist = await youtube.getPlaylist(playlistId).catch(() => {});
        if (playlist) {
            let videos: VideoCompact[];
            if (playlist instanceof Playlist) {
                // The first request only contains ~100 videos, load the rest in order.
                await playlist.videos.next(0).catch(() => null);
                videos = playlist.videos.items;
            } else {
                videos = playlist.videos;
            }

            const songs = videos.map(item => youtubeVideoToSong(item));
            const startAt = videoId === null ? -1 : songs.findIndex(song => song.id === videoId);
            if (startAt > 0) songs.unshift(...songs.splice(startAt, 1));

            result.items = songs;
            result.collectionName = playlist.title;
            if (songs.length > 0) return;
        }
        // Private/unavailable playlist: fall back to the video in the link, if any.
        if (videoId === null) {
            result.error = "privateOrUnavailable";
            return;
        }
    }

    if (videoId === null) {
        result.error = "unsupported";
        return;
    }

    const video = await youtube.getVideo(videoId).catch(() => {});
    if (video) {
        result.items = [
            {
                ...youtubeVideoToSong({
                    channel: video.channel ?? undefined,
                    duration: "duration" in video ? video.duration : 0,
                    id: video.id,
                    thumbnails: video.thumbnails,
                    title: video.title
                }),
                duration: video.isLiveContent ? 0 : ("duration" in video ? video.duration : 0)
            }
        ];
        return;
    }

    // youtubei can fail on age-restricted or region-locked videos; yt-dlp usually still works.
    const info = await getInfo(`https://www.youtube.com/watch?v=${videoId}`).catch(() => {});
    if (info) {
        result.items = [
            {
                duration: info.duration,
                id: info.id,
                source: "youtube",
                thumbnail: bestThumbnail(info.thumbnails),
                title: info.title,
                url: `https://www.youtube.com/watch?v=${videoId}`
            }
        ];
        return;
    }

    result.error = "privateOrUnavailable";
}

async function resolveSoundCloud(client: Rawon, url: URL, result: SearchTrackResult): Promise<void> {
    let scUrl = url;
    if (["www.soundcloud.app.goo.gl", "soundcloud.app.goo.gl", "on.soundcloud.com"].includes(url.hostname)) {
        const req = await client.request.get(url.toString());
        scUrl = new URL(req.url);
    }
    for (const key of scUrl.searchParams.keys()) scUrl.searchParams.delete(key);

    if (scUrl.pathname.includes("/sets/")) {
        const playlist = await client.soundcloud.playlists.fetch(await client.soundcloud.playlists.get(scUrl.toString()));
        result.items = playlist.tracks.map(item => soundcloudTrackToSong(item));
        result.collectionName = playlist.title;
        return;
    }

    const track = await client.soundcloud.tracks.get(scUrl.toString());
    result.items = [soundcloudTrackToSong(track)];
}

export async function searchTrack(
    client: Rawon,
    query: string,
    source: "soundcloud" | "youtube" | undefined = "youtube"
): Promise<SearchTrackResult> {
    const result: SearchTrackResult = {
        items: []
    };
    const trimmed = query.trim();
    const queryData = checkQuery(trimmed);

    if (queryData.isURL) {
        result.type = "results";

        try {
            switch (queryData.sourceType) {
                case "spotify": {
                    const resolved = await client.spotify.resolve(trimmed);
                    result.items = resolved.tracks;
                    result.collectionName = resolved.name;
                    break;
                }

                case "soundcloud":
                    await resolveSoundCloud(client, new URL(trimmed), result);
                    break;

                case "youtube":
                    await resolveYouTube(new URL(trimmed), result);
                    break;

                default: {
                    const info = await getInfo(trimmed).catch(() => {});
                    if (info) {
                        result.items = [
                            {
                                duration: info.duration,
                                id: info.id,
                                source: "other",
                                thumbnail: bestThumbnail(info.thumbnails),
                                title: info.title || "Unknown Song",
                                url: info.url || trimmed
                            }
                        ];
                    } else {
                        result.error = "unsupported";
                    }
                    break;
                }
            }
        } catch (error) {
            client.logger.error("SEARCH_TRACK_ERR:", error);
            result.error = error instanceof SpotifyError ? error.reason : "privateOrUnavailable";
        }
    } else {
        result.type = "selection";

        if (source === "soundcloud") {
            const searchRes = await client.soundcloud.tracks.search({
                // eslint-disable-next-line id-length
                q: trimmed
            });
            result.items = searchRes.collection.map(track => soundcloudTrackToSong(track));
        } else {
            const searchRes = await youtube.search(trimmed, { type: "video" });
            result.items = searchRes.items.filter(video => !video.isLive).map(video => youtubeVideoToSong(video));
        }
    }

    if (result.items.length === 0) result.error ??= "notFound";
    return result;
}
