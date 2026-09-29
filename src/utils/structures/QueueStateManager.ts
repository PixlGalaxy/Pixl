
import { mkdir, readdir, readFile, unlink, writeFile } from "node:fs/promises";
import path from "node:path";
import { clearTimeout, setInterval, setTimeout } from "node:timers";
import type { Rawon } from "../../structures/Rawon.js";
import type { LoopMode, Song } from "../../typings/index.js";
import type { filterArgs } from "../functions/ffmpegArgs.js";
import { OperationManager } from "./OperationManager.js";

export type PersistedSong = {
    index: number;
    originalIndex?: number;
    requesterId: string;
    song: Song;
};

export type PersistedQueue = {
    guildId: string;
    textChannelId: string;
    voiceChannelId: string;
    loopMode: LoopMode;
    shuffle: boolean;
    stayInVC: boolean;
    volume: number;
    filters: Partial<Record<keyof typeof filterArgs, boolean>>;
    songs: PersistedSong[];
    savedAt: number;
};

const saveDebounceMs = 2_000;
const snapshotIntervalMs = 30_000;
const maxEntryAgeMs = 86_400_000;

export class QueueStateManager {
    private readonly manager = new OperationManager();
    private saveTimeout: NodeJS.Timeout | null = null;
    private started = false;

    public constructor(private readonly client: Rawon, private readonly dataDir: string) { }

    public start(): void {
        if (this.started) return;
        this.started = true;
        setInterval(() => {
            if (this.hasQueues()) this.requestSave();
        }, snapshotIntervalMs).unref();
    }

    public requestSave(): void {
        if (this.saveTimeout) clearTimeout(this.saveTimeout);
        this.saveTimeout = setTimeout(() => {
            this.saveTimeout = null;
            void this.saveNow();
        }, saveDebounceMs);
    }

    public async saveNow(): Promise<void> {
        try {
            await this.manager.add(async () => {
                const snapshot = this.snapshot();
                await mkdir(this.dataDir, { recursive: true });
                await writeFile(this.filePath(), JSON.stringify(snapshot));
            });
        } catch (error) {
            this.client.logger.error("QUEUE_STATE_SAVE_ERR:", error);
        }
    }

    public async readAll(): Promise<PersistedQueue[]> {
        const entries: PersistedQueue[] = [];

        try {
            const files = await readdir(this.dataDir).catch(() => [] as string[]);
            for (const file of files.filter(fl => /^queues-shard\d+\.json$/u.test(fl))) {
                try {
                    const raw = await readFile(path.resolve(this.dataDir, file), "utf8");
                    const parsed = JSON.parse(raw) as Record<string, PersistedQueue>;
                    entries.push(...Object.values(parsed));
                } catch (error) {
                    this.client.logger.error("QUEUE_STATE_READ_ERR:", error);
                }
            }
        } catch (error) {
            this.client.logger.error("QUEUE_STATE_READ_ERR:", error);
        }

        const now = Date.now();
        return entries.filter(entry => now - entry.savedAt <= maxEntryAgeMs);
    }

    public async cleanOrphanFiles(): Promise<void> {
        const totalShards = this.client.shard?.count ?? 1;

        try {
            const files = await readdir(this.dataDir).catch(() => [] as string[]);
            for (const file of files) {
                const match = /^queues-shard(?<id>\d+)\.json$/u.exec(file);
                if (!match?.groups) continue;
                if (Number(match.groups.id) >= totalShards) {
                    await unlink(path.resolve(this.dataDir, file)).catch(() => null);
                }
            }
        } catch (error) {
            this.client.logger.error("QUEUE_STATE_CLEAN_ERR:", error);
        }
    }

    private hasQueues(): boolean {
        return this.client.guilds.cache.some(guild => guild.queue !== undefined);
    }

    private snapshot(): Record<string, PersistedQueue> {
        const result: Record<string, PersistedQueue> = {};

        for (const guild of this.client.guilds.cache.values()) {
            const queue = guild.queue;
            if (!queue) continue;

            const voiceChannelId = queue.connection?.joinConfig.channelId ?? null;
            if (voiceChannelId === null) continue;

            result[guild.id] = {
                guildId: guild.id,
                textChannelId: queue.textChannel.id,
                voiceChannelId,
                loopMode: queue.loopMode,
                shuffle: queue.shuffle,
                stayInVC: queue.stayInVC,
                volume: queue.volume,
                filters: queue.filters,
                songs: queue.songs
                    .sortByIndex()
                    .map(entry => ({
                        index: entry.index,
                        originalIndex: entry.originalIndex,
                        requesterId: entry.requester.id,
                        song: entry.song
                    })),
                savedAt: Date.now()
            };
        }

        return result;
    }

    private filePath(): string {
        const shardId = this.client.shard?.ids[0] ?? 0;
        return path.resolve(this.dataDir, `queues-shard${shardId}.json`);
    }
}
