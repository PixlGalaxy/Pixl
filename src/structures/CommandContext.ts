import { Buffer } from "node:buffer";
import type { BaseMessageOptions, ChatInputCommandInteraction, GuildMember, Interaction, InteractionEditReplyOptions, InteractionReplyOptions, InteractionResponse, MessageMentions, MessagePayload, MessageReplyOptions, ModalSubmitFields, StringSelectMenuInteraction, TextBasedChannel, User } from "discord.js";
import { ActionRowBuilder, BaseInteraction, ButtonBuilder, ButtonInteraction, ButtonStyle, Collection, CommandInteraction, ContextMenuCommandInteraction, Message, MessageComponentInteraction, MessageFlags, MessageFlagsBitField, MessagePayload as MessagePayloadClass, ModalSubmitInteraction, StringSelectMenuInteraction as StringSelectMenuInteractionClass } from "discord.js";
import type { MessageInteractionAction } from "../typings/index.js";
import { toV2 } from "../utils/functions/componentsV2.js";

export class CommandContext {
    public additionalArgs = new Collection<string, any>();
    // When true, interaction replies are only visible to the invoking user.
    public ephemeral = false;
    public channel: TextBasedChannel | null;
    public guild;

    public constructor(
        public readonly context:
            | CommandInteraction
            | ContextMenuCommandInteraction
            | Interaction
            | Message
            | StringSelectMenuInteraction,
        public args: string[] = []
    ) {
        this.channel = this.context.channel;
        this.guild = this.context.guild;
    }

    public async deferReply(): Promise<InteractionResponse | undefined> {
        if (this.isInteraction()) {
            return (this.context as CommandInteraction).deferReply();
        }
        return undefined;
    }

    public async reply(
        options:
            | BaseMessageOptions
            | InteractionReplyOptions
            | MessagePayload
            | string
            | { askDeletion?: { reference: string } },
        autoedit?: boolean
    ): Promise<Message> {
        const interaction = this.context as CommandInteraction;
        const action: MessageInteractionAction = this.isInteraction()
            ? interaction.replied
                ? autoedit === true
                    ? "editReply"
                    : "followUp"
                : interaction.deferred
                    ? "editReply"
                    : "reply"
            : "reply";
        const rep = await this.send(
            options,
            action
        ).catch((error: unknown) => ({ error }));
        if ("error" in rep) {
            throw new Error(`Unable to reply context, because: ${(rep.error as Error).message}`);
        }

        // @ts-expect-error-next-line
         
        return rep instanceof Message ? rep : new Message(this.context.client, rep);
    }

    public async send(
        options:
            | BaseMessageOptions
            | InteractionReplyOptions
            | MessagePayload
            | string
            | { askDeletion?: { reference: string } },
        type: MessageInteractionAction = "editReply"
    ): Promise<Message> {
        const deletionBtn = new ActionRowBuilder<ButtonBuilder>().addComponents(
            new ButtonBuilder().setEmoji("🗑️").setStyle(ButtonStyle.Danger)
        );
        if ((options as { askDeletion?: { reference: string } }).askDeletion) {
            deletionBtn.components[0].setCustomId(
                Buffer.from(
                    `${(options as { askDeletion: { reference: string } }).askDeletion.reference}_delete-msg`
                ).toString("base64")
            );
            (options as InteractionReplyOptions).components = [
                ...(options as InteractionReplyOptions).components ?? [],
                deletionBtn
            ];
        }
        if (this.isInteraction()) {
            if (this.ephemeral && type !== "editReply" && typeof options === "object" && !(options instanceof MessagePayloadClass)) {
                const flags = new MessageFlagsBitField((options as { flags?: number }).flags ?? 0).add(MessageFlags.Ephemeral);
                (options as { flags?: number }).flags = flags.bitfield;
            }
            // eslint-disable-next-line no-param-reassign
            options = toV2(options);
            const context = this.context as CommandInteraction;
            let msg: Message;
            if (type === "reply") {
                const response = await context.reply({
                    ...(typeof options === "string" ? { content: options } : options),
                    withResponse: true
                } as InteractionReplyOptions & { withResponse: true });
                const resourceMsg = response.resource?.message ?? null;
                if (resourceMsg === null) throw new Error("Unable to retrieve the interaction reply message.");
                msg = resourceMsg;
            } else if (type === "editReply") {
                msg = await context.editReply(options as InteractionEditReplyOptions | MessagePayload | string);
            } else {
                msg = await context.followUp(options as InteractionReplyOptions | MessagePayload | string);
            }
            const channel = this.context.channel;
            const res = await channel?.messages.fetch(msg.id).catch(() => null);
            return res ?? msg;
        }
        if ((options as Record<string, unknown>).ephemeral === true) {
            throw new Error("Cannot send ephemeral message in a non-interaction context.");
        }
        if (typeof options === "string") {
            // eslint-disable-next-line no-param-reassign
            options = { content: options };
        }

        // eslint-disable-next-line no-param-reassign
        options = toV2(options);
        ((options as MessageReplyOptions).allowedMentions ??= {}).repliedUser = false;
        return (this.context as Message).reply(options as MessageReplyOptions);
    }

    public isInteraction(): boolean {
        return this.context instanceof BaseInteraction;
    }

    public isCommand(): boolean {
        return this.context instanceof CommandInteraction;
    }

    public isContextMenu(): boolean {
        return this.context instanceof ContextMenuCommandInteraction;
    }

    public isMessageComponent(): boolean {
        return this.context instanceof MessageComponentInteraction;
    }

    public isButton(): boolean {
        return this.context instanceof ButtonInteraction;
    }

    public isStringSelectMenu(): boolean {
        return this.context instanceof StringSelectMenuInteractionClass;
    }

    public isModal(): boolean {
        return this.context instanceof ModalSubmitInteraction;
    }

    public get mentions(): MessageMentions | null {
        return this.context instanceof Message ? this.context.mentions : null;
    }

    public get deferred(): boolean {
        return this.context instanceof BaseInteraction ? (this.context as CommandInteraction).deferred : false;
    }

    public get options(): ChatInputCommandInteraction["options"] | null {
        /* Not sure about this but CommandInteraction does not provides getString method anymore */
        return this.context instanceof BaseInteraction ? (this.context as ChatInputCommandInteraction).options : null;
    }

    public get fields(): ModalSubmitFields | null {
        return this.context instanceof ModalSubmitInteraction ? this.context.fields : null;
    }

    public get author(): User {
        return this.context instanceof BaseInteraction ? this.context.user : this.context.author;
    }

    public get member(): GuildMember | null {
        return this.guild?.members.resolve(this.author.id) ?? null;
    }
}
