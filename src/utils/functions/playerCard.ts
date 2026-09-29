import type { AudioResource } from "@discordjs/voice";
import { AudioPlayerStatus } from "@discordjs/voice";
import {
    ActionRowBuilder,
    ButtonBuilder,
    ButtonStyle,
    ContainerBuilder,
    MessageFlags,
    SectionBuilder,
    SeparatorBuilder,
    SeparatorSpacingSize,
    ThumbnailBuilder,
    escapeMarkdown
} from "discord.js";
import i18n from "../../config/index.js";
import type { ServerQueue } from "../../structures/ServerQueue.js";
import type { QueueSong, SongSource } from "../../typings/index.js";
import { isMediaURL, textDisplay } from "./componentsV2.js";
import { colorOf } from "./createEmbed.js";
import { createProgressBar } from "./createProgressBar.js";
import { normalizeTime } from "./normalizeTime.js";
import { parseHTMLElements } from "./parseHTMLElements.js";

export const playerButtonPrefix = "rw:player:";

const sourceLabels: Record<SongSource, string> = {
    other: "🌐 Web",
    soundcloud: "☁️ SoundCloud",
    spotify: "🟢 Spotify",
    youtube: "▶️ YouTube"
};

const loopEmojis = { OFF: "➡️", QUEUE: "🔁", SONG: "🔂" } as const;

function button(action: string, emoji: string, style: ButtonStyle, label?: string, disabled = false): ButtonBuilder {
    const builder = new ButtonBuilder()
        .setCustomId(`${playerButtonPrefix}${action}`)
        .setEmoji(emoji)
        .setStyle(style)
        .setDisabled(disabled);
    if ((label?.length ?? 0) > 0) builder.setLabel(label ?? "");
    return builder;
}

export function currentSong(queue: ServerQueue): { song: QueueSong; resource: AudioResource } | null {
    const state = queue.player.state;
    if (state.status === AudioPlayerStatus.Idle) return null;
    const resource = (state as { resource?: AudioResource }).resource;
    const song = resource?.metadata as QueueSong | undefined;
    return resource && song ? { resource, song } : null;
}

// Builds the "now playing" panel: cover, progress, queue state and playback controls.
export function buildPlayerCard(
    queue: ServerQueue,
    disabled = false
): { components: ContainerBuilder[]; flags: MessageFlags.IsComponentsV2 } {
    const current = currentSong(queue);
    const container = new ContainerBuilder();

    if (!current) {
        container
            .setAccentColor(colorOf("info"))
            .addTextDisplayComponents(textDisplay(`⏹️ ${i18n.__("commands.music.nowplaying.emptyQueue")}`));
        return { components: [container], flags: MessageFlags.IsComponentsV2 };
    }

    const { resource, song } = current;
    const paused = queue.player.state.status === AudioPlayerStatus.Paused;
    const elapsed = Math.trunc(resource.playbackDuration / 1_000);
    const { duration } = song.song;
    const title = escapeMarkdown(parseHTMLElements(song.song.title));
    const lines = [
        `-# ${paused ? `⏸️ ${i18n.__("player.paused")}` : `🎶 ${i18n.__("player.nowPlaying")}`}`,
        `### [${title}](${song.song.url})`
    ];
    if ((song.song.artist?.length ?? 0) > 0) lines.push(`**${escapeMarkdown(song.song.artist ?? "")}**`);

    if (duration > 0) {
        const endsAt = Math.trunc((Date.now() + (duration - elapsed) * 1_000) / 1_000);
        lines.push(
            `\`${normalizeTime(elapsed)}\` ${createProgressBar(elapsed, duration)} \`${normalizeTime(duration)}\``,
            paused ? "" : `-# ${i18n.__mf("player.endsAt", { time: `<t:${endsAt}:R>` })}`
        );
    } else {
        lines.push(`🔴 ${i18n.__("player.live")}`);
    }
    lines.push(`-# ${i18n.__mf("player.requestedBy", { user: song.requester.toString() })}`);

    container.setAccentColor(colorOf(paused ? "warn" : "info"));
    const text = textDisplay(lines.filter(line => line.length > 0).join("\n"));
    if (isMediaURL(song.song.thumbnail)) {
        container.addSectionComponents(
            new SectionBuilder()
                .addTextDisplayComponents(text)
                .setThumbnailAccessory(new ThumbnailBuilder().setURL(song.song.thumbnail).setDescription(song.song.title.slice(0, 1_000)))
        );
    } else {
        container.addTextDisplayComponents(text);
    }

    const upcoming = queue.songs.filter(x => x.index > song.index).size;
    const next = queue.songs.sortByIndex().find(x => x.index > song.index);
    const on = i18n.__("player.on");
    const off = i18n.__("player.off");
    const status = [
        `${loopEmojis[queue.loopMode]} ${i18n.__("player.loop")}: **${queue.loopMode}**`,
        `🔀 ${i18n.__("player.shuffle")}: **${queue.shuffle ? on : off}**`,
        `🔊 **${queue.volume}%**`,
        `📜 ${i18n.__mf("player.inQueue", { count: upcoming })}`,
        sourceLabels[song.song.source ?? "youtube"]
    ].join(" • ");

    container
        .addSeparatorComponents(new SeparatorBuilder().setSpacing(SeparatorSpacingSize.Small))
        .addTextDisplayComponents(
            textDisplay(
                [
                    `-# ${status}`,
                    next ? `-# ⏭️ ${i18n.__("player.upNext")}: ${escapeMarkdown(parseHTMLElements(next.song.title)).slice(0, 80)}` : ""
                ]
                    .filter(Boolean)
                    .join("\n")
            )
        )
        .addActionRowComponents(
            new ActionRowBuilder<ButtonBuilder>().addComponents(
                button("toggle", paused ? "▶️" : "⏸️", ButtonStyle.Primary, paused ? i18n.__("player.resume") : i18n.__("player.pause"), disabled),
                button("skip", "⏭️", ButtonStyle.Secondary, i18n.__("player.skip"), disabled),
                button("stop", "⏹️", ButtonStyle.Danger, undefined, disabled),
                button("shuffle", "🔀", queue.shuffle ? ButtonStyle.Success : ButtonStyle.Secondary, undefined, disabled),
                button("loop", queue.loopMode === "SONG" ? "🔂" : "🔁", queue.loopMode === "OFF" ? ButtonStyle.Secondary : ButtonStyle.Success, undefined, disabled)
            ),
            new ActionRowBuilder<ButtonBuilder>().addComponents(
                button("voldown", "🔉", ButtonStyle.Secondary, undefined, disabled),
                button("volup", "🔊", ButtonStyle.Secondary, undefined, disabled),
                button("queue", "📜", ButtonStyle.Secondary, i18n.__("player.queue"), disabled),
                button("lyrics", "🎤", ButtonStyle.Secondary, i18n.__("player.lyrics"), disabled)
            )
        );

    return { components: [container], flags: MessageFlags.IsComponentsV2 };
}
