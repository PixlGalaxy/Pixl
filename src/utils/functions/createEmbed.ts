import type { ColorResolvable } from "discord.js";
import { EmbedBuilder } from "discord.js";
import { embedColor, noEmoji, yesEmoji } from "../../config/index.js";

type hexColorsType = "error" | "info" | "success" | "warn";
export const hexColors: Record<hexColorsType, string> = {
    error: "#ED4245",
    info: `#${embedColor}`,
    success: "#57F287",
    warn: "#FEE75C"
};

export function createEmbed(type: hexColorsType, message?: string, emoji = false): EmbedBuilder {
    const embed = new EmbedBuilder().setColor(hexColors[type] as ColorResolvable);

    if ((message?.length ?? 0) > 0) embed.setDescription(message ?? null);
    if (type === "error" && emoji) embed.setDescription(`${noEmoji} ${message}`);
    if (type === "success" && emoji) embed.setDescription(`${yesEmoji} ${message}`);
    return embed;
}

export function colorOf(type: hexColorsType): number {
    return Number.parseInt(hexColors[type].replace("#", ""), 16);
}
