import i18n from "../../config/index.js";
import { BaseCommand } from "../../structures/BaseCommand.js";
import type { CommandContext } from "../../structures/CommandContext.js";
import { Command } from "../../utils/decorators/Command.js";
import { haveQueue } from "../../utils/decorators/MusicUtil.js";
import { buildPlayerCard } from "../../utils/functions/playerCard.js";

@Command<typeof NowPlayingCommand>({
    aliases: ["np"],
    description: i18n.__("commands.music.nowplaying.description"),
    name: "nowplaying",
    slash: {
        options: []
    },
    usage: "{prefix}nowplaying"
})
export class NowPlayingCommand extends BaseCommand {
    @haveQueue
    public async execute(ctx: CommandContext): Promise<void> {
        const queue = ctx.guild?.queue;
        if (!queue) return;

        // The panel buttons are handled globally (InteractionCreateEvent), so they keep working
        // for as long as the message exists. Replace the old panel to avoid duplicated controls.
        const msg = await ctx.reply(buildPlayerCard(queue));
        if (!ctx.ephemeral) queue.lastMusicMsg = msg.id;
    }
}
