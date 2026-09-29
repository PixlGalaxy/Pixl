import type { BitFieldResolvable, ButtonInteraction, Interaction, PermissionsString, TextChannel } from "discord.js";
import { ApplicationCommandType, Message, PermissionsBitField } from "discord.js";
import i18n from "../config/index.js";
import { BaseEvent } from "../structures/BaseEvent.js";
import { CommandContext } from "../structures/CommandContext.js";
import type { LoopMode } from "../typings/index.js";
import { Event } from "../utils/decorators/Event.js";
import { toV2 } from "../utils/functions/componentsV2.js";
import { createEmbed } from "../utils/functions/createEmbed.js";
import { playerButtonPrefix } from "../utils/functions/playerCard.js";

const nextLoopMode: Record<LoopMode, string> = { OFF: "queue", QUEUE: "song", SONG: "disable" };

@Event("interactionCreate")
export class InteractionCreateEvent extends BaseEvent {
    public async execute(interaction: Interaction): Promise<void> {
        this.client.debugLog.logData("info", "INTERACTION_CREATE", [
            ["Type", interaction.type.toString()],
            ["Guild", interaction.inGuild() ? `${interaction.guild?.name ?? "[???]"}(${interaction.guildId})` : "DM"],
            [
                "Channel",
                (interaction.channel?.type ?? "DM") === "DM"
                    ? "DM"
                    : `${(interaction.channel as TextChannel).name}(${(interaction.channel as TextChannel).id})`
            ],
            ["User", `${interaction.user.tag}(${interaction.user.id})`]
        ]);

        if (!interaction.inGuild() || !this.client.commands.isReady) return;

        if (interaction.isAutocomplete()) {
            const cmd = this.client.commands.find(x => x.meta.slash?.name === interaction.commandName);
            await cmd?.autocomplete?.(interaction).catch((error: unknown) =>
                this.client.logger.error("AUTOCOMPLETE_ERR:", error)
            );
            return;
        }

        if (interaction.isButton() && interaction.customId.startsWith(playerButtonPrefix)) {
            await this.handlePlayerButton(interaction);
            return;
        }

        // Components with the `rw:` prefix belong to a message collector (pagination, etc.).
        if (interaction.isMessageComponent() && interaction.customId.startsWith("rw:")) return;
        if (interaction.isModalSubmit() && interaction.customId.startsWith("rw:")) return;

        if (interaction.isButton()) {
            const val = this.client.utils.decode(interaction.customId);
            const user = val.split("_")[0] ?? "";
            const cmd = val.split("_")[1] ?? "";

            if (cmd === "delete-msg") {
                if (
                    interaction.user.id !== user &&
                    !new PermissionsBitField(
                        interaction.member.permissions as BitFieldResolvable<PermissionsString, bigint> | undefined
                    ).has(PermissionsBitField.Flags.ManageMessages)
                ) {
                    void interaction.reply(
                        toV2({
                            ephemeral: true,
                            embeds: [
                                createEmbed(
                                    "error",
                                    i18n.__mf("events.createInteraction.message1", {
                                        user: user.toString()
                                    }),
                                    true
                                )
                            ]
                        })
                    );
                } else {
                    const msg = await interaction.channel?.messages.fetch(interaction.message.id).catch(() => null);
                    if (msg?.deletable === true) {
                        void msg.delete();
                    }
                }
            }
        }

        const context = new CommandContext(interaction);
        if (interaction.isUserContextMenuCommand() || interaction.isMessageContextMenuCommand()) {
            const data = interaction.isUserContextMenuCommand() ? interaction.targetUser : interaction.targetMessage;
            const dataType = data instanceof Message ? ApplicationCommandType.Message : ApplicationCommandType.User;

            const cmd = this.client.commands.find(x =>
                dataType === ApplicationCommandType.Message
                    ? x.meta.contextChat === interaction.commandName
                    : x.meta.contextUser === interaction.commandName
            );
            if (cmd) {
                context.additionalArgs.set("options", data);
                void cmd.execute(context);
            }
            return;
        }

        if (interaction.isChatInputCommand()) {
            const cmd = this.client.commands
                .filter(x => x.meta.slash !== undefined)
                .find(x => x.meta.slash?.name === interaction.commandName);
            if (cmd) {
                void cmd.execute(context);
            }
        }

        if (interaction.isStringSelectMenu()) {
            const val = this.client.utils.decode(interaction.customId);
            const user = val.split("_")[0] ?? "";
            const cmd = val.split("_")[1] ?? "";
            const exec = (val.split("_")[2] ?? "yes") === "yes";

            if (interaction.user.id !== user) {
                void interaction.reply(
                    toV2({
                        ephemeral: true,
                        embeds: [
                            createEmbed(
                                "error",
                                i18n.__mf("events.createInteraction.message1", {
                                    user: user.toString()
                                }),
                                true
                            )
                        ]
                    })
                );
            }
            if (cmd && user === interaction.user.id && exec) {
                const command = this.client.commands
                    .filter(x => x.meta.slash !== undefined)
                    .find(x => x.meta.name === cmd);
                if (command) {
                    context.additionalArgs.set("values", interaction.values);
                    void command.execute(context);
                }
            }
        }
    }

    /**
     * Buttons of the "now playing" panel. They reuse the regular commands, so permission checks
     * (same voice channel, DJ role, vote skip...) stay identical; answers are only shown to the clicker.
     */
    private async handlePlayerButton(interaction: ButtonInteraction): Promise<void> {
        const action = interaction.customId.slice(playerButtonPrefix.length);
        const queue = interaction.guild?.queue;
        const actions: Partial<Record<string, () => [string, string[]]>> = {
            loop: () => ["repeat", [nextLoopMode[queue?.loopMode ?? "OFF"]]],
            lyrics: () => ["lyrics", []],
            queue: () => ["queue", []],
            shuffle: () => ["shuffle", [queue?.shuffle === true ? "disable" : "enable"]],
            skip: () => ["skip", []],
            stop: () => ["stop", []],
            toggle: () => [queue?.playing === true ? "pause" : "resume", []],
            voldown: () => ["volume", [String(Math.max(1, (queue?.volume ?? 100) - 10))]],
            volup: () => ["volume", [String(Math.min(100, (queue?.volume ?? 100) + 10))]]
        };

        const resolved = actions[action]?.();
        const command = resolved ? this.client.commands.get(resolved[0]) : undefined;
        if (!resolved || !command) return;

        const ctx = new CommandContext(interaction, resolved[1]);
        ctx.ephemeral = true;
        try {
            await command.execute(ctx);
        } catch (error) {
            this.client.logger.error("PLAYER_BUTTON_ERR:", error);
        }
        interaction.guild?.queue?.refreshPlayerCard();
    }
}
