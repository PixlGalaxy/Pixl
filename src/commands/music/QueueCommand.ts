import { escapeMarkdown } from "discord.js";
import i18n from "../../config/index.js";
import { BaseCommand } from "../../structures/BaseCommand.js";
import { CommandContext } from "../../structures/CommandContext.js";
import { Command } from "../../utils/decorators/Command.js";
import { haveQueue } from "../../utils/decorators/MusicUtil.js";
import { chunk } from "../../utils/functions/chunk.js";
import { normalizeTime } from "../../utils/functions/normalizeTime.js";
import { parseHTMLElements } from "../../utils/functions/parseHTMLElements.js";
import { currentSong } from "../../utils/functions/playerCard.js";
import { ButtonPagination } from "../../utils/structures/ButtonPagination.js";

const perPage = 10;

@Command({
    aliases: ["q"],
    description: i18n.__("commands.music.queue.description"),
    name: "queue",
    slash: {
        options: []
    },
    usage: "{prefix}queue"
})
export class QueueCommand extends BaseCommand {
    @haveQueue
    public async execute(ctx: CommandContext): Promise<void> {
        const queue = ctx.guild?.queue;
        if (!queue) return;

        const np = currentSong(queue)?.song;
        // Positions match the ones used by the skipto/remove commands.
        const songs = [...queue.songs.sortByIndex().values()];
        const npPosition = songs.findIndex(song => song.key === np?.key);
        const totalDuration = songs.reduce((acc, song) => acc + song.song.duration, 0);

        const pages = chunk(songs, perPage).map((page, pageIndex) => page
            .map((song, i) => {
                const position = pageIndex * perPage + i + 1;
                const title = escapeMarkdown(parseHTMLElements(song.song.title)).slice(0, 70);
                const artist = (song.song.artist?.length ?? 0) > 0 ? ` — ${escapeMarkdown(song.song.artist ?? "").slice(0, 40)}` : "";
                const duration = song.song.duration > 0 ? ` \`${normalizeTime(song.song.duration)}\`` : "";
                const line = `[${title}](${song.song.url})${artist}${duration}`;

                return song.key === np?.key ? `▶️ **${position}. ${line}**` : `\`${position}.\` ${line}`;
            })
            .join("\n"));

        const flags = [
            queue.shuffle ? `🔀 ${i18n.__("player.shuffle")}` : "",
            queue.loopMode === "OFF" ? "" : `${queue.loopMode === "SONG" ? "🔂" : "🔁"} ${queue.loopMode}`
        ].filter(Boolean).join(" • ");

        await ButtonPagination.send(ctx, {
            author: ctx.author.id,
            footer: [i18n.__mf("player.totalSongs", { count: songs.length }), `⏱️ ${normalizeTime(totalDuration)}`, flags]
                .filter(Boolean)
                .join(" • "),
            pages,
            select: {
                placeholder: i18n.__("commands.music.queue.selectPlaceholder"),
                options: pageIndex => songs.slice(pageIndex * perPage, (pageIndex + 1) * perPage).map((song, i) => {
                    const position = pageIndex * perPage + i + 1;
                    return {
                        description: [song.song.artist, song.song.duration > 0 ? normalizeTime(song.song.duration) : ""]
                            .filter(Boolean)
                            .join(" • ")
                            .slice(0, 100) || undefined,
                        emoji: song.key === np?.key ? "▶️" : undefined,
                        label: `${position}. ${parseHTMLElements(song.song.title)}`.slice(0, 100),
                        value: String(position)
                    };
                }),
                onSelect: async (interaction, values) => {
                    const skipCtx = new CommandContext(interaction, [values[0] ?? ""]);
                    skipCtx.ephemeral = true;
                    await this.client.commands.get("skipto")?.execute(skipCtx);
                }
            },
            startPage: npPosition === -1 ? 0 : Math.floor(npPosition / perPage),
            thumbnail: np?.song.thumbnail ?? ctx.guild?.iconURL({ extension: "png", size: 512 }),
            title: `📜 ${i18n.__mf("commands.music.queue.title", { guild: escapeMarkdown(ctx.guild?.name ?? "") })}`
        });
    }
}
