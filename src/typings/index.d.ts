/* eslint-disable typescript/consistent-type-definitions */
/* eslint-disable typescript/naming-convention */
import type { ApplicationCommandOptionData, ApplicationCommandType, AutocompleteInteraction, Client as OClient, ClientEvents, ClientPresenceStatus, Collection, Guild as OG, GuildMember, StringSelectMenuInteraction } from "discord.js";
import type { CommandContext } from "../structures/CommandContext.js";
import type { Rawon } from "../structures/Rawon.js";
import type { ServerQueue } from "../structures/ServerQueue.js";

export type MessageInteractionAction = "editReply" | "followUp" | "reply";

export type QueryData = {
    sourceType?: "query" | "soundcloud" | "spotify" | "unknown" | "youtube";
    type?: "playlist" | "track" | "unknown";
    isURL: boolean;
}

export type SearchTrackError = "notFound" | "privateOrUnavailable" | "spotifyUnavailable" | "unsupported";

export type BasicYoutubeVideoInfo = {
    thumbnails?: { url: string; width: number; height: number }[];
    duration: number;
    title: string;
    url: string;
    id: string;
}

export type SearchTrackResult = {
    type?: "results" | "selection";
    items: Song[];
    // Name of the playlist/album when the query resolved to a collection.
    collectionName?: string;
    error?: SearchTrackError;
}

export type PaginationPayload = {
    // User allowed to flip pages.
    author: string;
    pages: string[];
    title?: string;
    thumbnail?: string | null;
    color?: number;
    // Extra footer text shown next to the page counter.
    footer?: string;
    // Page shown first (0-based).
    startPage?: number;
    // Wrap each page in a code block.
    codeBlock?: boolean;
    // Optional select menu built for the current page.
    select?: {
        placeholder: string;
        options(pageIndex: number): { label: string; value: string; description?: string; emoji?: string }[];
        onSelect(interaction: StringSelectMenuInteraction, values: string[]): Promise<unknown> | unknown;
    };
}

export type RawonLoggerOptions = {
    prod: boolean;
}

export type SlashOption = {
    options?: ApplicationCommandOptionData[];
    type?: ApplicationCommandType;
    defaultPermission?: boolean;
    description?: string;
    name?: string;
}

export type EnvActivityTypes = "Competing" | "Listening" | "Playing" | "Watching";

export type PresenceData = {
    activities: { name: string; type: EnvActivityTypes }[];
    status: ClientPresenceStatus[];
    interval: number;
}

export type Event = {
    readonly name: keyof ClientEvents;
    execute(...args: any): void;
}

export type CommandComponent = {
    execute(context: CommandContext): any;
    autocomplete?(interaction: AutocompleteInteraction): Promise<void>;
    meta: {
        readonly category?: string;
        readonly path?: string;
        contextChat?: string;
        contextUser?: string;
        description?: string;
        slash?: SlashOption;
        aliases?: string[];
        cooldown?: number;
        disable?: boolean;
        devOnly?: boolean;
        usage?: string;
        name: string;
    };
}

export type CategoryMeta = {
    cmds: Collection<string, CommandComponent>;
    hide: boolean;
    name: string;
}

declare module "discord.js" {
    export interface Client extends OClient {
        commands: Rawon["commands"];
        request: Rawon["request"];
        config: Rawon["config"];
        logger: Rawon["logger"];
        events: Rawon["events"];

        build(): Promise<this>;
    }

    export interface Guild extends OG {
        queue?: ServerQueue;
        client: Rawon;
    }
}

export type SongSource = "other" | "soundcloud" | "spotify" | "youtube";

export type Song = {
    thumbnail: string;
    // Duration in seconds.
    duration: number;
    title: string;
    // Public URL shown to users (e.g. the Spotify link for Spotify tracks).
    url: string;
    id: string;
    artist?: string;
    source?: SongSource;
    // Direct playable URL. Spotify tracks get it lazily right before playing.
    streamUrl?: string;
    // Search hints used to find a playable source for Spotify tracks.
    isrc?: string;
}

export type QueueSong = {
    requester: GuildMember;
    // Play order. Rewritten when the queue is shuffled.
    index: number;
    // Order in which the song was added, used to restore after shuffling.
    originalIndex: number;
    song: Song;
    key: string;
}

export type LoopMode = "OFF" | "QUEUE" | "SONG";

export type LyricsAPIResult<E extends boolean> = {
    synced: E extends true ? never : boolean | string;
    album_art?: E extends true ? null : string;
    message?: E extends true ? string : never;
    artist?: E extends true ? null : string;
    lyrics?: E extends true ? null : string;
    song?: E extends true ? null : string;
    url?: E extends true ? null : string;
    error: E;
}

export type SpotifyAccessTokenAPIResult = {
    accessTokenExpirationTimestampMs: number;
    accessToken?: string;
    isAnonymous: boolean;
    clientId: string;
}

export type ExternalUrls = {
    spotify: string;
}

export type ArtistsEntity = {
    external_urls: ExternalUrls;
    href: string;
    name: string;
    type: string;
    uri: string;
    id: string;
}

export type SpotifyImage = {
    url: string;
    width?: number | null;
    height?: number | null;
}

export type SpotifyTrack = {
    artists: { name: string; id?: string }[];
    duration_ms: number;
    external_ids?: {
        isrc?: string;
    };
    external_urls?: {
        spotify?: string;
    };
    album?: {
        name?: string;
        images?: SpotifyImage[];
    };
    is_local?: boolean;
    type?: string;
    name: string;
    id: string;
}

export type SpotifyPage<T> = {
    items: T[];
    next: string | null;
}

// Playlist entries used `track` historically and `item` after the 2026 Web API changes.
export type SpotifyPlaylistEntry = {
    track?: SpotifyTrack | null;
    item?: SpotifyTrack | null;
    is_local?: boolean;
}

export type SpotifyCollection = {
    name: string;
    images?: SpotifyImage[];
    tracks?: SpotifyPage<SpotifyPlaylistEntry | SpotifyTrack>;
    items?: SpotifyPage<SpotifyPlaylistEntry | SpotifyTrack>;
}

export type GuildData = {
    dj?: {
        enable: boolean;
        role: string | null;
    };
    infractions: Record<
        string,
        {
            on: number;
            reason: string | null;
        }[]
    >;
    modLog?: {
        enable: boolean;
        channel: string | null;
    };
    mute?: string | null;
}

export type NonAbstractConstructor<Result = unknown> = new (...args: any[]) => Result;
export type Constructor<Result = unknown> = NonAbstractConstructor<Result> | (abstract new (...args: any[]) => Result);

export type MethodDecorator<Target, Result> = (
    target: Target,
    propertyKey: string,
    descriptor: PropertyDescriptor
) => Result;
export type ClassDecorator<Target extends Constructor, Result = unknown> = (target: Target) => Result;
export type Promisable<Output> = Output | Promise<Output>;
export type FunctionType<Args extends any[] = any[], Result = any> = (...args: Args) => Result;
