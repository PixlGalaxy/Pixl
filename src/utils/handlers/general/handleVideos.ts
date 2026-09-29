import type { DiscordGatewayAdapterCreator} from "@discordjs/voice";
import { joinVoiceChannel } from "@discordjs/voice";
import type { Message, StageChannel, TextChannel, VoiceChannel } from "discord.js";
import { escapeMarkdown } from "discord.js";
import i18n from "../../../config/index.js";
import type { CommandContext } from "../../../structures/CommandContext.js";
import type { Rawon } from "../../../structures/Rawon.js";
import { ServerQueue } from "../../../structures/ServerQueue.js";
import type { Song } from "../../../typings/index.js";
import { chunk } from "../../functions/chunk.js";
import { toV2 } from "../../functions/componentsV2.js";
import { colorOf, createEmbed } from "../../functions/createEmbed.js";
import { normalizeTime } from "../../functions/normalizeTime.js";
import { parseHTMLElements } from "../../functions/parseHTMLElements.js";
import { ButtonPagination } from "../../structures/ButtonPagination.js";
import { play } from "./play.js";

export async function handleVideos(
    client: Rawon,
    ctx: CommandContext,
    toQueue: Song[],
    voiceChannel: StageChannel | VoiceChannel,
    collectionName?: string
): Promise<Message | undefined> {
    const wasIdle = ctx.guild?.queue?.idle;

    async function sendPagination(): Promise<void> {
        const queue = ctx.guild?.queue;
        const member = ctx.member as unknown as NonNullable<typeof ctx.member>;
        const shuffleAfter = queue?.shuffle === true ? queue.currentIndex : undefined;
        const keys = toQueue.map(song => queue?.songs.addSong(song, member, shuffleAfter));

        const format = (song: Song): string => {
            const title = escapeMarkdown(parseHTMLElements(song.title));
            const artist = (song.artist?.length ?? 0) > 0 ? ` — ${escapeMarkdown(song.artist ?? "")}` : "";
            const duration = song.duration > 0 ? ` \`${normalizeTime(song.duration)}\`` : "";
            return `[${title}](${song.url})${artist}${duration}`;
        };

        if (toQueue.length === 1) {
            const [song] = toQueue;
            const added = queue?.songs.get(keys[0] ?? "");
            const current = queue?.currentIndex ?? -1;
            const position = queue?.songs.filter(x => x.index > current && x.index <= (added?.index ?? 0)).size ?? 1;
            const embed = createEmbed(
                "success",
                `${format(song)}\n-# ${i18n.__mf("utils.generalHandler.queuePosition", { position: Math.max(1, position) })}`
            )
                .setAuthor({ name: `✅ ${i18n.__("utils.generalHandler.addedToQueue")}` })
                .setThumbnail(song.thumbnail || null);
            await ctx.reply({ embeds: [embed] }, true);
            return;
        }

        const totalDuration = toQueue.reduce((acc, song) => acc + song.duration, 0);
        const pages = chunk(toQueue, 10).map((vals, i) => vals
            .map((song, index) => `\`${i * 10 + (index + 1)}.\` ${format(song)}`)
            .join("\n"));

        await ButtonPagination.send(ctx, {
            author: ctx.author.id,
            color: colorOf("success"),
            footer: `⏱️ ${normalizeTime(totalDuration)}`,
            pages,
            thumbnail: toQueue[0]?.thumbnail,
            title: `✅ ${i18n.__mf("utils.generalHandler.handleVideoInitial", { length: toQueue.length })}${
                (collectionName?.length ?? 0) > 0 ? ` · ${escapeMarkdown(collectionName ?? "")}` : ""
            }`
        }, true);
    }

    if (ctx.guild?.queue) {
        await sendPagination();

        if (wasIdle === true) {
            void play(ctx.guild, undefined, wasIdle);
        }

        return;
    }

    (ctx.guild as unknown as NonNullable<typeof ctx.guild>).queue = new ServerQueue(ctx.channel as TextChannel);
    await sendPagination();

    client.debugLog.logData("info", "HANDLE_VIDEOS", `Created a server queue for ${ctx.guild?.name}(${ctx.guild?.id})`);

    try {
        const connection = joinVoiceChannel({
            adapterCreator: ctx.guild?.voiceAdapterCreator as DiscordGatewayAdapterCreator,
            channelId: voiceChannel.id,
            guildId: ctx.guild?.id ?? "",
            selfDeaf: true
        }).on("debug", message => {
            client.logger.debug(message);
        });

        (ctx.guild?.queue as unknown as NonNullable<NonNullable<typeof ctx.guild>["queue"]>).connection = connection;

        client.debugLog.logData(
            "info",
            "HANDLE_VIDEOS",
            `Connected to ${voiceChannel.name}(${voiceChannel.id}) in guild ${ctx.guild?.name}(${ctx.guild?.id})`
        );
    } catch (error) {
        ctx.guild?.queue?.songs.clear();
        delete ctx.guild?.queue;

        client.debugLog.logData(
            "error",
            "HANDLE_VIDEOS",
            `Error occured while connecting to ${ctx.guild?.name}(${ctx.guild?.id}). Reason: ${(error as Error).message
            }`
        );

        client.logger.error("PLAY_CMD_ERR:", error);
        await (ctx.channel as TextChannel | null)
            ?.send(toV2({
                embeds: [
                    createEmbed(
                        "error",
                        i18n.__mf("utils.generalHandler.errorJoining", { message: `\`${(error as Error).message}\`` }),
                        true
                    )
                ]
            }))
            // eslint-disable-next-line typescript/naming-convention
            .catch((error_: unknown) => {
                client.logger.error("PLAY_CMD_ERR:", error_);
            });
        return;
    }

    void play(ctx.guild as unknown as NonNullable<typeof ctx.guild>);
}
