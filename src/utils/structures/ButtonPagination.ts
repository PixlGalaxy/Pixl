import type { ButtonInteraction, Message, StringSelectMenuInteraction } from "discord.js";
import {
    ActionRowBuilder,
    ButtonBuilder,
    ButtonStyle,
    ContainerBuilder,
    LabelBuilder,
    MessageFlags,
    ModalBuilder,
    SectionBuilder,
    SeparatorBuilder,
    SeparatorSpacingSize,
    StringSelectMenuBuilder,
    TextInputBuilder,
    TextInputStyle,
    ThumbnailBuilder
} from "discord.js";
import i18n from "../../config/index.js";
import type { CommandContext } from "../../structures/CommandContext.js";
import type { PaginationPayload } from "../../typings/index.js";
import { isMediaURL, maxV2TextLength, textDisplay } from "../functions/componentsV2.js";
import { colorOf } from "../functions/createEmbed.js";

// Component ids starting with `rw:` are handled by their own collectors, not by the global handler.
const ids = {
    first: "rw:pg:first",
    prev: "rw:pg:prev",
    jump: "rw:pg:jump",
    next: "rw:pg:next",
    last: "rw:pg:last",
    select: "rw:pg:select"
} as const;

export class ButtonPagination {
    private index = 0;
    private msg!: Message;

    public constructor(public readonly payload: PaginationPayload) {
        this.index = Math.min(Math.max(0, payload.startPage ?? 0), this.pageCount - 1);
    }

    // Replies to the command with the first page and starts listening for page changes.
    public static async send(ctx: CommandContext, payload: PaginationPayload, autoedit = false): Promise<Message> {
        const pagination = new ButtonPagination(payload);
        const msg = await ctx.reply(pagination.render(false), autoedit);
        pagination.listen(msg);
        return msg;
    }

    public get pageCount(): number {
        return Math.max(1, this.payload.pages.length);
    }

    public listen(msg: Message): void {
        this.msg = msg;
        if (this.pageCount < 2 && !this.payload.select) return;

        const collector = this.msg.createMessageComponentCollector({ idle: 180_000 });

        collector.on("collect", async interaction => {
            if (!interaction.isButton() && !interaction.isStringSelectMenu()) return;
            if (interaction.user.id !== this.payload.author) {
                await interaction
                    .reply({
                        components: [
                            textDisplay(
                                `🔒 ${i18n.__mf("reusable.pagination.notYours", { author: `<@${this.payload.author}>` })}`
                            )
                        ],
                        flags: MessageFlags.IsComponentsV2 | MessageFlags.Ephemeral
                    })
                    .catch(() => null);
                return;
            }

            switch (interaction.customId) {
                case ids.first:
                    this.index = 0;
                    break;
                case ids.prev:
                    this.index = Math.max(0, this.index - 1);
                    break;
                case ids.next:
                    this.index = Math.min(this.pageCount - 1, this.index + 1);
                    break;
                case ids.last:
                    this.index = this.pageCount - 1;
                    break;
                case ids.jump:
                    await this.askPage(interaction as ButtonInteraction);
                    return;
                case ids.select:
                    await this.payload.select?.onSelect(
                        interaction as StringSelectMenuInteraction,
                        (interaction as StringSelectMenuInteraction).values
                    );
                    if (!interaction.replied && !interaction.deferred) {
                        await interaction.update(this.render(false)).catch(() => null);
                    }
                    return;
                default:
                    return;
            }

            await interaction.update(this.render(false)).catch(() => null);
        });

        collector.on("end", async () => {
            // Ephemeral messages cannot be edited this way; they simply expire.
            await this.msg.edit(this.render(true)).catch(() => null);
        });
    }

    private async askPage(interaction: ButtonInteraction): Promise<void> {
        const modalId = `rw:pg:modal:${interaction.id}`;
        await interaction.showModal(
            new ModalBuilder()
                .setCustomId(modalId)
                .setTitle(i18n.__("reusable.pagination.jumpTitle"))
                .addLabelComponents(
                    new LabelBuilder()
                        .setLabel(i18n.__mf("reusable.pagination.jumpLabel", { total: this.pageCount }))
                        .setTextInputComponent(
                            new TextInputBuilder()
                                .setCustomId("page")
                                .setStyle(TextInputStyle.Short)
                                .setPlaceholder(`1 - ${this.pageCount}`)
                                .setMinLength(1)
                                .setMaxLength(6)
                                .setRequired(true)
                        )
                )
        );

        const submit = await interaction
            .awaitModalSubmit({ filter: modal => modal.customId === modalId, time: 60_000 })
            .catch(() => null);
        if (!submit) return;

        const page = Number.parseInt(submit.fields.getTextInputValue("page"), 10);
        if (Number.isNaN(page) || page < 1 || page > this.pageCount) {
            await submit
                .reply({
                    components: [textDisplay(`⚠️ ${i18n.__mf("reusable.pagination.invalidPage", { total: this.pageCount })}`)],
                    flags: MessageFlags.IsComponentsV2 | MessageFlags.Ephemeral
                })
                .catch(() => null);
            return;
        }

        this.index = page - 1;
        if (submit.isFromMessage()) {
            await submit.update(this.render(false)).catch(() => null);
        } else {
            await submit.deferUpdate().catch(() => null);
            await interaction.editReply(this.render(false)).catch(() => null);
        }
    }

    public render(disabled: boolean): { components: ContainerBuilder[]; flags: MessageFlags.IsComponentsV2 } {
        const { codeBlock, color, footer, select, thumbnail, title } = this.payload;
        const page = this.payload.pages[this.index] ?? "";
        const body = [
            (title?.length ?? 0) > 0 ? `### ${title}` : "",
            codeBlock === true ? `\`\`\`\n${page}\n\`\`\`` : page
        ]
            .filter(Boolean)
            .join("\n")
            .slice(0, maxV2TextLength - 200);

        const container = new ContainerBuilder().setAccentColor(color ?? colorOf("info"));
        if (isMediaURL(thumbnail)) {
            container.addSectionComponents(
                new SectionBuilder()
                    .addTextDisplayComponents(textDisplay(body))
                    .setThumbnailAccessory(new ThumbnailBuilder().setURL(thumbnail))
            );
        } else {
            container.addTextDisplayComponents(textDisplay(body));
        }

        container.addSeparatorComponents(new SeparatorBuilder().setSpacing(SeparatorSpacingSize.Small));
        const pageFooter = i18n.__mf("reusable.pageFooter", { actual: this.index + 1, total: this.pageCount });
        container.addTextDisplayComponents(
            textDisplay(`-# ${pageFooter}${(footer?.length ?? 0) > 0 ? ` • ${footer}` : ""}`)
        );

        const options = select?.options(this.index).slice(0, 25) ?? [];
        if (select && options.length > 0) {
            container.addActionRowComponents(
                new ActionRowBuilder<StringSelectMenuBuilder>().addComponents(
                    new StringSelectMenuBuilder()
                        .setCustomId(ids.select)
                        .setPlaceholder(select.placeholder)
                        .setDisabled(disabled)
                        .addOptions(options)
                )
            );
        }

        if (this.pageCount > 1) {
            const atStart = this.index === 0;
            const atEnd = this.index === this.pageCount - 1;
            container.addActionRowComponents(
                new ActionRowBuilder<ButtonBuilder>().addComponents(
                    new ButtonBuilder().setCustomId(ids.first).setEmoji("⏮️").setStyle(ButtonStyle.Secondary).setDisabled(disabled || atStart),
                    new ButtonBuilder().setCustomId(ids.prev).setEmoji("◀️").setStyle(ButtonStyle.Primary).setDisabled(disabled || atStart),
                    new ButtonBuilder()
                        .setCustomId(ids.jump)
                        .setLabel(`${this.index + 1} / ${this.pageCount}`)
                        .setEmoji("🔢")
                        .setStyle(ButtonStyle.Secondary)
                        .setDisabled(disabled),
                    new ButtonBuilder().setCustomId(ids.next).setEmoji("▶️").setStyle(ButtonStyle.Primary).setDisabled(disabled || atEnd),
                    new ButtonBuilder().setCustomId(ids.last).setEmoji("⏭️").setStyle(ButtonStyle.Secondary).setDisabled(disabled || atEnd)
                )
            );
        }

        return { components: [container], flags: MessageFlags.IsComponentsV2 };
    }
}
