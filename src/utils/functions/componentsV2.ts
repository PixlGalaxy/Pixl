import type { APIEmbed, APIMessageTopLevelComponent, JSONEncodable } from "discord.js";
import {
    ContainerBuilder,
    EmbedBuilder,
    MediaGalleryBuilder,
    MediaGalleryItemBuilder,
    MessageFlags,
    MessageFlagsBitField,
    MessagePayload,
    SectionBuilder,
    SeparatorBuilder,
    SeparatorSpacingSize,
    TextDisplayBuilder,
    ThumbnailBuilder
} from "discord.js";

// Discord caps the combined text of every Text Display in a message at 4000 characters.
export const maxV2TextLength = 4_000;

type TextBudget = { left: number };
type AnyComponent = APIMessageTopLevelComponent | JSONEncodable<APIMessageTopLevelComponent>;

function omit(source: Record<string, unknown>, keys: string[]): Record<string, unknown> {
    return Object.fromEntries(Object.entries(source).filter(([key]) => !keys.includes(key)));
}

export function isMediaURL(url: string | null | undefined): url is string {
    return typeof url === "string" && /^(?:https?|attachment):\/\//u.test(url);
}

export function textDisplay(content: string): TextDisplayBuilder {
    return new TextDisplayBuilder().setContent(content.length > 0 ? content : "​");
}

function take(budget: TextBudget, text: string): string {
    const limit = Math.max(0, budget.left);
    const result = text.length > limit ? `${text.slice(0, Math.max(0, limit - 1))}…` : text;
    budget.left -= result.length;
    return result;
}

/**
 * Renders a classic embed as a Components V2 container, so every message of the bot
 * shares the same modern look (accent color, section thumbnail, subtle footer).
 */
export function embedToContainer(
    input: APIEmbed | EmbedBuilder,
    textBudget?: TextBudget
): ContainerBuilder {
    const budget = textBudget ?? { left: maxV2TextLength };
    const data = input instanceof EmbedBuilder ? input.toJSON() : input;
    const container = new ContainerBuilder();
    let children = 0;

    if (typeof data.color === "number") container.setAccentColor(data.color);

    const body: string[] = [];
    if ((data.author?.name.length ?? 0) > 0) body.push(`-# ${data.author?.name}`);
    if ((data.title?.length ?? 0) > 0) {
        body.push(`### ${(data.url?.length ?? 0) > 0 ? `[${data.title}](${data.url})` : data.title}`);
    }
    if ((data.description?.length ?? 0) > 0) body.push(data.description ?? "");

    const bodyText = take(budget, body.join("\n"));
    const thumbnail = data.thumbnail?.url;
    if (bodyText.length > 0) {
        if (isMediaURL(thumbnail)) {
            container.addSectionComponents(
                new SectionBuilder()
                    .addTextDisplayComponents(textDisplay(bodyText))
                    .setThumbnailAccessory(new ThumbnailBuilder().setURL(thumbnail))
            );
        } else {
            container.addTextDisplayComponents(textDisplay(bodyText));
        }
        children++;
    }

    if ((data.fields?.length ?? 0) > 0 && budget.left > 0) {
        // Consecutive short inline fields share a line, the rest get their own block.
        const blocks: string[] = [];
        let inline: string[] = [];
        for (const field of data.fields ?? []) {
            if (field.inline === true && !field.value.includes("\n")) {
                inline.push(`**${field.name}** ${field.value}`);
                continue;
            }
            if (inline.length > 0) blocks.push(inline.join("  •  "));
            inline = [];
            blocks.push(`**${field.name}**\n${field.value}`);
        }
        if (inline.length > 0) blocks.push(inline.join("  •  "));
        const fields = blocks.join("\n\n");
        if (children > 0) {
            container.addSeparatorComponents(new SeparatorBuilder().setDivider(false).setSpacing(SeparatorSpacingSize.Small));
        }
        container.addTextDisplayComponents(textDisplay(take(budget, fields)));
        children++;
    }

    if (isMediaURL(data.image?.url)) {
        container.addMediaGalleryComponents(
            new MediaGalleryBuilder().addItems(new MediaGalleryItemBuilder().setURL(data.image?.url ?? ""))
        );
        children++;
    }

    const footer: string[] = [];
    if ((data.footer?.text.length ?? 0) > 0) footer.push(data.footer?.text ?? "");
    if ((data.timestamp?.length ?? 0) > 0) {
        footer.push(`<t:${Math.trunc(new Date(data.timestamp ?? "").getTime() / 1_000)}:R>`);
    }
    if (footer.length > 0 && budget.left > 0) {
        if (children > 0) container.addSeparatorComponents(new SeparatorBuilder().setSpacing(SeparatorSpacingSize.Small));
        container.addTextDisplayComponents(textDisplay(take(budget, `-# ${footer.join(" • ")}`)));
        children++;
    }

    if (children === 0) container.addTextDisplayComponents(textDisplay("​"));

    return container;
}

/**
 * Converts a legacy `{ content, embeds, components }` payload into a Components V2 payload.
 * Payloads without text or embeds (e.g. component-only edits) and already-converted
 * payloads are returned untouched.
 */
export function toV2<T>(options: T): T {
    if (typeof options === "string") {
        return {
            components: [textDisplay(options.slice(0, maxV2TextLength))],
            flags: MessageFlags.IsComponentsV2
        } as unknown as T;
    }
    if (options === null || typeof options !== "object" || options instanceof MessagePayload) return options;

    const opts = options as Record<string, unknown>;
    const embeds = (opts.embeds as (APIEmbed | EmbedBuilder)[] | undefined) ?? [];
    const content = typeof opts.content === "string" ? opts.content.trim() : "";
    const flags = new MessageFlagsBitField((opts.flags as number | undefined) ?? 0);

    if (embeds.length === 0 && content.length === 0) {
        if (opts.ephemeral !== true) return options;
        // `ephemeral` is deprecated in discord.js; express it as a flag instead.
        flags.add(MessageFlags.Ephemeral);
        return { ...omit(opts, ["ephemeral"]), flags: flags.bitfield } as unknown as T;
    }

    const budget: TextBudget = { left: maxV2TextLength };
    const components: AnyComponent[] = [];
    if (content.length > 0) components.push(textDisplay(take(budget, content)));
    for (const embed of embeds) components.push(embedToContainer(embed, budget));
    components.push(...((opts.components as AnyComponent[] | undefined) ?? []));

    flags.add(MessageFlags.IsComponentsV2);
    if (opts.ephemeral === true) flags.add(MessageFlags.Ephemeral);

    return { ...omit(opts, ["content", "embeds", "ephemeral"]), components, flags: flags.bitfield } as unknown as T;
}
