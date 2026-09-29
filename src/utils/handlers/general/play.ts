import { pipeline } from "node:stream";
import type { Readable } from "node:stream";
import { clearTimeout, setTimeout } from "node:timers";
import { createAudioResource, entersState, StreamType, VoiceConnectionStatus } from "@discordjs/voice";
import type { Guild } from "discord.js";
import { ChannelType, escapeMarkdown } from "discord.js";
import prism from "prism-media";
import i18n from "../../../config/index.js";
import type { QueueSong } from "../../../typings/index.js";
import { toV2 } from "../../functions/componentsV2.js";
import { createEmbed } from "../../functions/createEmbed.js";
import { ffmpegArgs } from "../../functions/ffmpegArgs.js";
import { getStream } from "../YTDLUtil.js";
import { ensurePlayable } from "./resolveSong.js";

async function skipBrokenSong(guild: Guild, song: QueueSong, error: unknown, wasIdle?: boolean): Promise<void> {
    const queue = guild.queue;
    if (!queue) return;

    queue.client.logger.warn(`PLAY_HANDLER: skipping "${song.song.title}": ${(error as Error).message}`);
    await queue.textChannel
        .send(
            toV2({
                embeds: [
                    createEmbed(
                        "warn",
                        `⚠️ ${i18n.__mf("utils.generalHandler.noPlayableSource", {
                            song: `**${escapeMarkdown(song.song.title)}**`
                        })}`
                    )
                ]
            })
        )
        .catch(() => null);

    const next = queue.songs.sortByIndex().find(x => x.index > song.index);
    queue.songs.delete(song.key);
    await play(guild, (next ?? queue.songs.sortByIndex().first())?.key ?? "", wasIdle);
}

export async function play(guild: Guild, nextSong?: string, wasIdle?: boolean): Promise<void> {
    const queue = guild.queue;
    if (!queue) return;

    const song = (nextSong?.length ?? 0) > 0
        ? queue.songs.get(nextSong as unknown as string)
        : nextSong === undefined
            ? queue.songs.sortByIndex().first()
            : undefined;

    clearTimeout(queue.dcTimeout ?? undefined);
    if (!song) {
        queue.lastMusicMsg = null;
        queue.lastVSUpdateMsg = null;
        void queue.setVoiceStatus(null);
        void queue.textChannel.send(
            toV2({
                embeds: [
                    createEmbed(
                        "info",
                        `⏹ ${i18n.__mf("utils.generalHandler.queueEnded", {
                            usage: `\`${guild.client.config.mainPrefix}play\``
                        })}`
                    )
                ]
            })
        );
        queue.dcTimeout = queue.stayInVC
            ? null
            : setTimeout(async () => {
                queue.destroy();
                await queue.textChannel
                    .send(toV2({ embeds: [createEmbed("info", `👋 ${i18n.__("utils.generalHandler.leftVC")}`)] }))
                    .then(msg => {
                        setTimeout(() => {
                            void msg.delete().catch(() => null);
                        }, 3_500);
                        return 0;
                    })
                    .catch(() => null);
            }, 60_000);
        queue.client.debugLog.logData("info", "PLAY_HANDLER", `Queue ended for ${guild.name}(${guild.id})`);
        return;
    }

    let source: Readable;
    try {
        const streamUrl = await ensurePlayable(song.song);
        source = await getStream(queue.client, streamUrl);
    } catch (error) {
        await skipBrokenSong(guild, song, error, wasIdle);
        return;
    }

    const stream = new prism.FFmpeg({
        args: ffmpegArgs(queue.filters)
    });
    pipeline(source, stream as unknown as NodeJS.WritableStream, () => {
        if (!source.destroyed) source.destroy();
    });

    const resource = createAudioResource(stream, { inlineVolume: true, inputType: StreamType.OggOpus, metadata: song });

    queue.client.debugLog.logData("info", "PLAY_HANDLER", `Created audio resource for ${guild.name}(${guild.id})`);

    queue.connection?.subscribe(queue.player);

    async function playResource(): Promise<void> {
        if (guild.channels.cache.get(queue?.connection?.joinConfig.channelId ?? "")?.type === ChannelType.GuildStageVoice) {
            queue?.client.debugLog.logData(
                "info",
                "PLAY_HANDLER",
                `Trying to be a speaker in ${guild.members.me?.voice.channel?.name ?? "Unknown"}(${guild.members.me?.voice.channel?.id ?? "ID UNKNOWN"
                }) in guild ${guild.name}(${guild.id})`
            );
            const suppressed = await guild.members.me?.voice
                .setSuppressed(false)
                .catch((error: unknown) => ({ error }));
            if (suppressed && "error" in suppressed) {
                queue?.client.debugLog.logData(
                    "error",
                    "PLAY_HANDLER",
                    `Failed to be a speaker in ${guild.members.me?.voice.channel?.name ?? "Unknown"}(${guild.members.me?.voice.channel?.id ?? "ID UNKNOWN"
                    }) in guild ${guild.name}(${guild.id}). Reason: ${(suppressed.error as Error).message}`
                );
                throw suppressed.error as Error;
            }
        }

        queue?.player.play(resource);
    }

    try {
        if (wasIdle !== true) {
            queue.client.debugLog.logData(
                "info",
                "PLAY_HANDLER",
                `Trying to enter Ready state in guild ${guild.name}(${guild.id}) voice connection`
            );
            await entersState(queue.connection as unknown as NonNullable<typeof queue.connection>, VoiceConnectionStatus.Ready, 15_000)
                .catch((error: unknown) => {
                    if ((error as Error).message === "The operation was aborted.") {
                        throw new Error("Cannot establish a voice connection within 15 seconds.");
                    }
                    throw error;
                });
        }
        await playResource();
    } catch (error) {
        source.destroy();
        queue.client.debugLog.logData(
            "error",
            "PLAY_HANDLER",
            `Failed to start playback in guild ${guild.name}(${guild.id}). Reason: ${(error as Error).message}`
        );
        await queue.textChannel
            .send(
                toV2({
                    embeds: [
                        createEmbed(
                            "error",
                            i18n.__mf("utils.generalHandler.errorPlaying", { message: `\`${(error as Error).message}\`` }),
                            true
                        )
                    ]
                })
            )
            .catch(() => null);
        queue.destroy();
    }
}
