import { ApplicationCommandOptionType } from "discord.js";
import i18n from "../../config/index.js";
import { BaseCommand } from "../../structures/BaseCommand.js";
import type { CommandContext } from "../../structures/CommandContext.js";
import { Command } from "../../utils/decorators/Command.js";
import { haveQueue, inVC, sameVC } from "../../utils/decorators/MusicUtil.js";
import { createEmbed } from "../../utils/functions/createEmbed.js";

@Command({
    description: i18n.__("commands.music.shuffle.description"),
    name: "shuffle",
    slash: {
        options: [
            {
                choices: [
                    {
                        name: "ENABLE",
                        value: "enable"
                    },
                    {
                        name: "DISABLE",
                        value: "disable"
                    }
                ],
                description: i18n.__("commands.music.shuffle.description"),
                name: "state",
                required: false,
                type: ApplicationCommandOptionType.String
            }
        ]
    },
    usage: "{prefix}shuffle [enable | disable]"
})
export class ShuffleCommand extends BaseCommand {
    @inVC
    @haveQueue
    @sameVC
    public async execute(ctx: CommandContext): Promise<void> {
        const queue = ctx.guild?.queue;
        if (!queue) return;

        const requested = (ctx.options?.getString("state") ?? ctx.args[0] ?? "").toLowerCase();
        // Without an explicit state the command toggles shuffle, like the player button.
        const enable = ["enable", "on", "yes"].includes(requested)
            ? true
            : ["disable", "off", "no"].includes(requested)
                ? false
                : !queue.shuffle;

        queue.shuffle = enable;
        const upcoming = queue.songs.filter(x => x.index > queue.currentIndex).size;

        await ctx.reply({
            embeds: [
                createEmbed(
                    "success",
                    `${enable ? "🔀" : "▶"} ${i18n.__mf("commands.music.shuffle.newState", {
                        state: `\`${enable ? "ENABLED" : "DISABLED"}\``
                    })}\n-# ${i18n.__mf(enable ? "commands.music.shuffle.shuffledInfo" : "commands.music.shuffle.restoredInfo", {
                        count: upcoming
                    })}`
                )
            ]
        });
    }
}
