import { clearTimeout, setTimeout } from "node:timers";
import type { AudioPlayer, AudioPlayerPlayingState, AudioResource, VoiceConnection } from "@discordjs/voice";
import { AudioPlayerStatus, createAudioPlayer } from "@discordjs/voice";
import type { TextChannel, Snowflake } from "discord.js";
import { ChannelType, PermissionFlagsBits } from "discord.js";
import i18n from "../config/index.js";
import type { LoopMode, QueueSong } from "../typings/index.js";
import { toV2 } from "../utils/functions/componentsV2.js";
import { createEmbed } from "../utils/functions/createEmbed.js";
import type { filterArgs } from "../utils/functions/ffmpegArgs.js";
import { buildPlayerCard, currentSong } from "../utils/functions/playerCard.js";
import { play } from "../utils/handlers/GeneralUtil.js";
import { ensurePlayable } from "../utils/handlers/general/resolveSong.js";
import { SongManager } from "../utils/structures/SongManager.js";
import type { Rawon } from "./Rawon.js";

const nonEnum = { enumerable: false };
const maxConsecutiveErrors = 3;

export class ServerQueue {
    public stayInVC = this.client.config.stayInVCAfterFinished;
    public readonly player: AudioPlayer = createAudioPlayer();
    public connection: VoiceConnection | null = null;
    public dcTimeout: NodeJS.Timeout | null = null;
    public timeout: NodeJS.Timeout | null = null;
    public readonly songs: SongManager;
    public filters: Partial<Record<keyof typeof filterArgs, boolean>> = {};

    private _volume = this.client.config.defaultVolume;
    private _loopMode: LoopMode = "OFF";
    private _shuffle = false;
    private _lastVSUpdateMsg: Snowflake | null = null;
    private _lastMusicMsg: Snowflake | null = null;
    private _skipVoters: Snowflake[] = [];
    private _refreshTimeout: NodeJS.Timeout | null = null;
    private _consecutiveErrors = 0;
    private _voiceStatus: string | null = null;

    public constructor(public readonly textChannel: TextChannel) {
        Object.defineProperties(this, {
            _skipVoters: nonEnum,
            _lastMusicMsg: nonEnum,
            _lastVSUpdateMsg: nonEnum,
            _volume: nonEnum,
            _loopMode: nonEnum,
            _shuffle: nonEnum,
            _refreshTimeout: nonEnum,
            _consecutiveErrors: nonEnum,
            _voiceStatus: nonEnum
        });

        this.songs = new SongManager(this.client, this.textChannel.guild);

        this.player
            .on("stateChange", async (oldState, newState) => {
                if (newState.status === AudioPlayerStatus.Playing && oldState.status !== AudioPlayerStatus.Paused) {
                    newState.resource.volume?.setVolumeLogarithmic(this.volume / 100);

                    const newSong = (newState.resource.metadata as QueueSong).song;
                    this.sendStartPlayingMsg(newSong);
                    void this.setVoiceStatus(
                        `🎶 ${newSong.title}${(newSong.artist?.length ?? 0) > 0 ? ` — ${newSong.artist}` : ""}`
                    );
                    this.prefetchNext();
                } else if (
                    (newState.status === AudioPlayerStatus.Paused && oldState.status === AudioPlayerStatus.Playing) ||
                    (newState.status === AudioPlayerStatus.Playing && oldState.status === AudioPlayerStatus.Paused)
                ) {
                    this.refreshPlayerCard();
                } else if (newState.status === AudioPlayerStatus.Idle && oldState.status !== AudioPlayerStatus.Idle) {
                    const song = (oldState as AudioPlayerPlayingState).resource.metadata as QueueSong;
                    this.client.logger.info(
                        `${this.client.shard ? `[Shard #${this.client.shard.ids[0]}]` : ""} Track: "${song.song.title
                        }" on ${this.textChannel.guild.name} has ended.`
                    );
                    this.skipVoters = [];
                    if (this.loopMode === "OFF") {
                        this.songs.delete(song.key);
                    }

                    await play(this.textChannel.guild, this.nextSongKey(song)).catch(async (error: unknown) => {
                        await this.textChannel
                            .send(
                                toV2({
                                    embeds: [
                                        createEmbed(
                                            "error",
                                            i18n.__mf("utils.generalHandler.errorPlaying", {
                                                message: `\`${(error as Error).message}\``
                                            }),
                                            true
                                        )
                                    ]
                                })
                            )
                            .catch((sendError: unknown) => this.client.logger.error("PLAY_ERR:", sendError));
                        this.client.logger.error("PLAY_ERR:", error);
                        this.destroy();
                    });
                }
            })
            .on("error", err => {
                // The player goes back to Idle by itself after an error, so the next song is played
                // by the stateChange handler. Only give up when several songs fail in a row.
                this._consecutiveErrors++;
                this.client.logger.error("PLAY_ERR:", err);
                void this.textChannel
                    .send(
                        toV2({
                            embeds: [
                                createEmbed(
                                    "error",
                                    i18n.__mf("utils.generalHandler.errorPlaying", { message: `\`${err.message}\`` }),
                                    true
                                )
                            ]
                        })
                    )
                    .catch((error: unknown) => this.client.logger.error("PLAY_CMD_ERR:", error));
                if (this._consecutiveErrors >= maxConsecutiveErrors) this.destroy();
            })
            .on("debug", message => {
                this.client.logger.debug(message);
            });
    }

    // Key of the song that should play after `song` finished, or "" when the queue is over.
    public nextSongKey(song: QueueSong): string {
        if (this.loopMode === "SONG" && this.songs.has(song.key)) return song.key;

        const after = this.songs.sortByIndex().find(x => x.index > song.index);
        if (after) return after.key;
        if (this.songs.size === 0) return "";

        // Loop OFF: songs jumped over with skipto are still pending. Loop QUEUE: start a new round.
        return (this.loopMode === "QUEUE" && this.shuffle
            ? this.songs.reshuffleAll(song.key)
            : this.songs.sortByIndex().first()
        )?.key ?? "";
    }

    public setFilter(filter: keyof typeof filterArgs, state: boolean): void {
        const before = this.filters[filter];
        this.filters[filter] = state;

        if (before !== state && this.player.state.status === AudioPlayerStatus.Playing) {
            this.playing = false;
            void play(this.textChannel.guild, (this.player.state.resource as AudioResource<QueueSong>).metadata.key, true);
        }
    }

    public stop(): void {
        this.songs.clear();
        this.player.stop(true);
    }

    public destroy(): void {
        void this.setVoiceStatus(null);
        this.stop();
        this.connection?.disconnect();
        clearTimeout(this.timeout ?? undefined);
        clearTimeout(this.dcTimeout ?? undefined);
        clearTimeout(this._refreshTimeout ?? undefined);
        delete this.textChannel.guild.queue;
        this.client.queueState.requestSave();
    }

    // Index of the song currently loaded in the player, or -1.
    public get currentIndex(): number {
        return currentSong(this)?.song.index ?? -1;
    }

    public get loopMode(): LoopMode {
        return this._loopMode;
    }

    public set loopMode(value: LoopMode) {
        this._loopMode = value;
        this.refreshPlayerCard();
    }

    public get shuffle(): boolean {
        return this._shuffle;
    }

    // Turning shuffle on reorders the upcoming songs; turning it off restores the original order.
    public set shuffle(value: boolean) {
        this.setShuffle(value, true);
    }

    public setShuffle(value: boolean, reorder: boolean): void {
        const changed = this._shuffle !== value;
        this._shuffle = value;
        if (changed && reorder) {
            if (value) this.songs.shuffleUpcoming(this.currentIndex);
            else this.songs.restoreUpcoming(this.currentIndex);
        }
        this.refreshPlayerCard();
    }

    public get volume(): number {
        return this._volume;
    }

    public set volume(newVol: number) {
        this._volume = newVol;
        (
            this.player.state as AudioPlayerPlayingState & { resource: AudioResource | undefined }
        ).resource?.volume?.setVolumeLogarithmic(this._volume / 100);
        this.refreshPlayerCard();
    }

    public get skipVoters(): Snowflake[] {
        return this._skipVoters;
    }

    public set skipVoters(value: Snowflake[]) {
        this._skipVoters = value;
    }

    public get lastMusicMsg(): Snowflake | null {
        return this._lastMusicMsg;
    }

    public set lastMusicMsg(value: Snowflake | null) {
        if (this._lastMusicMsg !== null && this._lastMusicMsg !== value) {
            const previous = this._lastMusicMsg;
            void this.quietly(this.textChannel.messages.delete(previous), "DELETE_LAST_MUSIC_MESSAGE_ERR");
        }
        this._lastMusicMsg = value;
    }

    public get lastVSUpdateMsg(): Snowflake | null {
        return this._lastVSUpdateMsg;
    }

    public set lastVSUpdateMsg(value: Snowflake | null) {
        if (this._lastVSUpdateMsg !== null && this._lastVSUpdateMsg !== value) {
            const previous = this._lastVSUpdateMsg;
            void this.quietly(this.textChannel.messages.delete(previous), "DELETE_LAST_VS_UPDATE_MESSAGE_ERR");
        }
        this._lastVSUpdateMsg = value;
    }

    public get playing(): boolean {
        return this.player.state.status === AudioPlayerStatus.Playing;
    }

    public set playing(value: boolean) {
        if (value) {
            this.player.unpause();
        } else {
            this.player.pause();
        }
    }

    public get idle(): boolean {
        return this.player.state.status === AudioPlayerStatus.Idle && this.songs.size === 0;
    }

    public get client(): Rawon {
        return this.textChannel.client as Rawon;
    }

    // Re-renders the player panel in place (debounced to stay far from rate limits).
    public refreshPlayerCard(): void {
        if (this._lastMusicMsg === null) return;
        clearTimeout(this._refreshTimeout ?? undefined);
        this._refreshTimeout = setTimeout(() => {
            this._refreshTimeout = null;
            if (this._lastMusicMsg === null || !currentSong(this)) return;
            void this.quietly(this.textChannel.messages.edit(this._lastMusicMsg, buildPlayerCard(this)), "REFRESH_PLAYER_CARD_ERR");
        }, 750);
    }

    /**
     * Shows the current song in the voice channel status (Discord's "voice channel status" feature).
     * Needs the Set Voice Channel Status permission; silently skipped otherwise.
     */
    public async setVoiceStatus(status: string | null): Promise<void> {
        if (!this.client.config.enableVoiceStatus) return;
        const value = status === null ? null : status.slice(0, 500);
        if (value === this._voiceStatus) return;

        const channelId = this.connection?.joinConfig.channelId;
        const channel = this.textChannel.guild.channels.cache.get(channelId ?? "");
        const me = this.textChannel.guild.members.me;
        if (channel?.type !== ChannelType.GuildVoice || !me) return;
        if (!channel.permissionsFor(me).has(PermissionFlagsBits.SetVoiceChannelStatus)) return;

        this._voiceStatus = value;
        await this.client.rest
            .put(`/channels/${channel.id}/voice-status`, { body: { status: value } })
            .catch((error: unknown) => this.client.logger.debug(`VOICE_STATUS_ERR: ${(error as Error).message}`));
    }

    // Resolves the next Spotify track in the background so the transition is seamless.
    private prefetchNext(): void {
        const current = currentSong(this);
        if (!current) return;
        const next = this.songs.sortByIndex().find(x => x.index > current.song.index);
        if (next) void this.quietly(ensurePlayable(next.song), "PREFETCH_ERR");
    }

    private sendStartPlayingMsg(newSong: QueueSong["song"]): void {
        this._consecutiveErrors = 0;
        this.client.logger.info(
            `${this.client.shard ? `[Shard #${this.client.shard.ids[0]}]` : ""} Track: "${newSong.title}" on ${this.textChannel.guild.name
            } has started.`
        );
        void (async () => {
            try {
                const msg = await this.textChannel.send(buildPlayerCard(this));
                this.lastMusicMsg = msg.id;
            } catch (error) {
                this.client.logger.error("PLAY_ERR:", error);
            }
        })();
    }

    private async quietly(task: Promise<unknown>, tag: string): Promise<void> {
        try {
            await task;
        } catch (error) {
            this.client.logger.debug(`${tag}: ${(error as Error).message}`);
        }
    }
}
