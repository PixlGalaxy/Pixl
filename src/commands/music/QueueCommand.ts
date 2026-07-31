import { AudioPlayerPlayingState, AudioResource } from "@discordjs/voice";
import i18n from "../../config/index.js";
import { BaseCommand } from "../../structures/BaseCommand.js";
import { CommandContext } from "../../structures/CommandContext.js";
import { QueueSong } from "../../typings/index.js";
import { Command } from "../../utils/decorators/Command.js";
import { haveQueue } from "../../utils/decorators/MusicUtil.js";
import { chunk } from "../../utils/functions/chunk.js";
import { createEmbed } from "../../utils/functions/createEmbed.js";
import { ButtonPagination } from "../../utils/structures/ButtonPagination.js";
import { SongManager } from "../../utils/structures/SongManager.js";

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
        const np = (
            ctx.guild?.queue?.player.state as (AudioPlayerPlayingState & { resource: AudioResource | undefined }) | undefined
        )?.resource?.metadata as QueueSong | undefined;
        const full = ctx.guild?.queue?.songs.sortByIndex() as unknown as SongManager;
        const songs = ctx.guild?.queue?.loopMode === "QUEUE" || np === undefined
            ? full
            : full.filter(val => val.index >= np.index);
        const pages = chunk([...songs.values()], 10).map((sngs, ind) => sngs
            .map((song, i) => {
                const addition = song.key === np?.key ? "**" : "";

                return `${addition}${ind * 10 + (i + 1)} - [${song.song.title}](${song.song.url})${addition}`;
            })
            .join("\n"));
        const embed = createEmbed("info", pages[0]).setThumbnail(ctx.guild?.iconURL({ extension: "png", size: 1_024 }) ?? null);
        const msg = await ctx.reply({ embeds: [embed] });

        return new ButtonPagination(msg, {
            author: ctx.author.id,
            edit: (i, emb, page) =>
                emb.setDescription(page).setFooter({
                    text: i18n.__mf("reusable.pageFooter", {
                        actual: i + 1,
                        total: pages.length
                    })
                }),
            embed,
            pages
        }).start();
    }
}
