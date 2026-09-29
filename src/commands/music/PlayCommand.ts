import { setTimeout } from "node:timers";
import type { AutocompleteInteraction, Message, VoiceBasedChannel } from "discord.js";
import { ApplicationCommandOptionType } from "discord.js";
import i18n from "../../config/index.js";
import { BaseCommand } from "../../structures/BaseCommand.js";
import type { CommandContext } from "../../structures/CommandContext.js";
import type { SearchTrackError, Song } from "../../typings/index.js";
import { Command } from "../../utils/decorators/Command.js";
import { inVC, sameVC, validVC } from "../../utils/decorators/MusicUtil.js";
import { createEmbed } from "../../utils/functions/createEmbed.js";
import { mapWithConcurrency } from "../../utils/functions/mapWithConcurrency.js";
import { normalizeTime } from "../../utils/functions/normalizeTime.js";
import { handleVideos, searchTrack } from "../../utils/handlers/GeneralUtil.js";
import { youtube } from "../../utils/handlers/YouTubeUtil.js";
import { checkQuery } from "../../utils/handlers/general/checkQuery.js";

const errorKeys: Record<SearchTrackError, string> = {
    notFound: "commands.music.play.noSongData",
    privateOrUnavailable: "commands.music.play.privateOrUnavailable",
    spotifyUnavailable: "commands.music.play.spotifyUnavailable",
    unsupported: "commands.music.play.unsupportedURL"
};

function truncate(text: string, max: number): string {
    return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

@Command({
    aliases: ["p", "add"],
    description: i18n.__("commands.music.play.description"),
    name: "play",
    slash: {
        description: i18n.__("commands.music.play.description"),
        options: [
            {
                autocomplete: true,
                description: i18n.__("commands.music.play.slashQueryDescription"),
                name: "query",
                type: ApplicationCommandOptionType.String,
                required: true
            }
        ]
    },
    usage: i18n.__("commands.music.play.usage")
})
export class PlayCommand extends BaseCommand {
    // Suggests YouTube results while the user types. Must answer within 3 seconds.
    public async autocomplete(interaction: AutocompleteInteraction): Promise<void> {
        const focused = interaction.options.getFocused().trim();
        if (focused.length < 2 || checkQuery(focused).isURL) {
            await interaction.respond(focused.length > 0 ? [{ name: truncate(focused, 100), value: truncate(focused, 100) }] : []).catch(() => null);
            return;
        }

        const results = await Promise.race([
            youtube.search(focused, { type: "video" }).then(res => res.items).catch(() => []),
            new Promise<[]>(resolve => {
                setTimeout(() => resolve([]), 2_300);
            })
        ]);

        const choices = results
            .filter(video => !video.isLive)
            .slice(0, 10)
            .map(video => {
                const duration = (video.duration ?? 0) > 0 ? ` (${normalizeTime(video.duration ?? 0)})` : "";
                const channel = (video.channel?.name.length ?? 0) > 0 ? ` — ${video.channel?.name}` : "";
                return {
                    name: truncate(`${truncate(video.title, 70)}${channel}${duration}`, 100),
                    value: `https://www.youtube.com/watch?v=${video.id}`
                };
            });

        await interaction
            .respond(choices.length > 0 ? choices : [{ name: truncate(focused, 100), value: truncate(focused, 100) }])
            .catch(() => null);
    }

    @inVC
    @validVC
    @sameVC
    public async execute(ctx: CommandContext): Promise<Message | undefined> {
        if (ctx.isInteraction() && !ctx.deferred) await ctx.deferReply();

        const voiceChannel = ctx.member?.voice.channel as unknown as VoiceBasedChannel;
        if (ctx.additionalArgs.get("fromSearch") !== undefined) {
            const tracks = ctx.additionalArgs.get("values") as string[];
            const results = await mapWithConcurrency(tracks, 4, async track => searchTrack(this.client, track));
            const toQueue = results.map(result => result?.items[0]).filter((song): song is Song => song !== undefined);

            if (toQueue.length === 0) {
                return ctx.reply({ embeds: [createEmbed("error", i18n.__("commands.music.play.noSongData"), true)] });
            }
            return handleVideos(this.client, ctx, toQueue, voiceChannel);
        }

        const query =
            (ctx.args.join(" ") || ctx.options?.getString("query")) ??
            (ctx.additionalArgs.get("values") === undefined
                ? undefined
                : (ctx.additionalArgs.get("values") as (string | undefined)[])[0]);

        if ((query?.length ?? 0) === 0) {
            return ctx.reply({
                embeds: [
                    createEmbed(
                        "warn",
                        i18n.__mf("reusable.invalidUsage", {
                            prefix: `${this.client.config.mainPrefix}help`,
                            name: this.meta.name
                        })
                    )
                ]
            });
        }

        if (ctx.guild?.queue && voiceChannel.id !== ctx.guild.queue.connection?.joinConfig.channelId) {
            return ctx.reply({
                embeds: [
                    createEmbed(
                        "warn",
                        i18n.__mf("commands.music.play.alreadyPlaying", {
                            voiceChannel:
                                ctx.guild.channels.cache.get(
                                    (ctx.guild.queue.connection?.joinConfig as { channelId: string }).channelId
                                )?.name ?? "#unknown-channel"
                        })
                    )
                ]
            });
        }

        const songs = await searchTrack(this.client, query ?? "").catch((error: unknown) => {
            this.client.logger.error("PLAY_CMD_SEARCH_ERR:", error);
            
        });
        if (!songs || songs.items.length === 0) {
            return ctx.reply({
                embeds: [createEmbed("error", i18n.__(errorKeys[songs?.error ?? "notFound"]), true)]
            });
        }

        return handleVideos(
            this.client,
            ctx,
            songs.type === "results" ? songs.items : [songs.items[0]],
            voiceChannel,
            songs.collectionName
        );
    }
}
