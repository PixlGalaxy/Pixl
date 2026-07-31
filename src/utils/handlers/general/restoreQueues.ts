import type { DiscordGatewayAdapterCreator } from "@discordjs/voice";
import { joinVoiceChannel } from "@discordjs/voice";
import { ChannelType } from "discord.js";
import type { Rawon } from "../../../structures/Rawon.js";
import { ServerQueue } from "../../../structures/ServerQueue.js";
import { play } from "./play.js";

export async function restoreQueues(client: Rawon): Promise<void> {
    const entries = await client.queueState.readAll();

    for (const entry of entries) {
        const guild = client.guilds.cache.get(entry.guildId);
        if (!guild || guild.queue) continue;
        if (entry.songs.length === 0) continue;

        const textChannel = guild.channels.cache.get(entry.textChannelId);
        const voiceChannel = guild.channels.cache.get(entry.voiceChannelId);
        if (textChannel?.type !== ChannelType.GuildText || voiceChannel?.isVoiceBased() !== true) continue;

        const listeners = voiceChannel.members.filter(member => !member.user.bot);
        if (listeners.size === 0) {
            client.logger.info(
                `Skipping queue restore for ${guild.name}(${guild.id}), no listeners left in the voice channel.`
            );
            continue;
        }

        try {
            const requesterIds = [...new Set(entry.songs.map(song => song.requesterId))];
            const fetched = await guild.members.fetch({ user: requesterIds }).catch(() => null);
            const fallback = guild.members.me;
            if (!fallback) continue;

            const queue = new ServerQueue(textChannel);
            guild.queue = queue;
            queue.loopMode = entry.loopMode;
            queue.shuffle = entry.shuffle;
            queue.stayInVC = entry.stayInVC;
            queue.filters = entry.filters;
            queue.volume = entry.volume;

            for (const song of [...entry.songs].sort((a, b) => a.index - b.index)) {
                queue.songs.addSong(song.song, fetched?.get(song.requesterId) ?? fallback);
            }

            queue.connection = joinVoiceChannel({
                adapterCreator: guild.voiceAdapterCreator as DiscordGatewayAdapterCreator,
                channelId: voiceChannel.id,
                guildId: guild.id,
                selfDeaf: true
            });

            client.logger.info(
                `Restored queue for ${guild.name}(${guild.id}) with ${entry.songs.length} songs, resuming playback.`
            );
            await play(guild);
        } catch (error) {
            client.logger.error("QUEUE_RESTORE_ERR:", error);
            guild.queue?.destroy();
        }
    }
}
