import type { GuildMember, Snowflake } from "discord.js";
import { Collection, SnowflakeUtil } from "discord.js";
import type { Rawon } from "../../structures/Rawon.js";
import type { Song, QueueSong } from "../../typings/index.js";

// Unbiased in-place Fisher-Yates shuffle.
export function fisherYates<T>(items: T[], random: () => number = Math.random): T[] {
    for (let i = items.length - 1; i > 0; i--) {
        const swapIndex = Math.floor(random() * (i + 1));
        [items[i], items[swapIndex]] = [items[swapIndex], items[i]];
    }
    return items;
}

export class SongManager extends Collection<Snowflake, QueueSong> {
    private id = 0;

    public constructor(public readonly client: Rawon, public readonly guild: GuildMember["guild"]) {
        super();
    }

    /**
     * Adds a song at the end of the queue. When `shuffleAfter` is a number (the index of the
     * song currently playing), the new song is dropped into a random upcoming position instead.
     */
    public addSong(song: Song, requester: GuildMember, shuffleAfter?: number): Snowflake {
        const key = SnowflakeUtil.generate().toString();
        const index = this.id++;
        const data: QueueSong = {
            index,
            key,
            originalIndex: index,
            requester,
            song
        };

        this.set(key, data);

        if (shuffleAfter !== undefined) {
            const upcoming = this.filter(x => x.index > shuffleAfter).map(x => x);
            const target = upcoming[Math.floor(Math.random() * upcoming.length)];
            if (target !== undefined && target.key !== key) {
                [target.index, data.index] = [data.index, target.index];
            }
        }

        return key;
    }

    // Randomizes the play order of every song after `afterIndex`, keeping the current one in place.
    public shuffleUpcoming(afterIndex = -1): void {
        const upcoming = this.filter(x => x.index > afterIndex).map(x => x);
        const slots = upcoming.map(x => x.index).sort((a, b) => a - b);
        for (const [i, song] of fisherYates(upcoming).entries()) {
            song.index = slots[i];
        }
        this.client.queueState.requestSave();
    }

    // Puts every song after `afterIndex` back in the order it was added.
    public restoreUpcoming(afterIndex = -1): void {
        const upcoming = this.filter(x => x.index > afterIndex).map(x => x);
        const slots = upcoming.map(x => x.index).sort((a, b) => a - b);
        for (const [i, song] of upcoming
            .sort((a, b) => a.originalIndex - b.originalIndex).entries()) {
                song.index = slots[i];
            }
        this.client.queueState.requestSave();
    }

    /**
     * Reshuffles the whole queue for a new loop round and returns the first song, making sure it
     * is not the one that just finished (when there is more than one song).
     */
    public reshuffleAll(lastKey?: string): QueueSong | undefined {
        const all = this.map(x => x);
        const slots = all.map(x => x.index).sort((a, b) => a - b);
        fisherYates(all);
        if (all.length > 1 && all[0].key === lastKey) [all[0], all[1]] = [all[1], all[0]];
        for (const [i, song] of all.entries()) {
            song.index = slots[i];
        }
        this.client.queueState.requestSave();
        return all[0];
    }

    public set(key: Snowflake, data: QueueSong): this {
        (this.client as Rawon | undefined)?.debugLog.logData(
            "info",
            "SONG_MANAGER",
            `New value added to ${this.guild.name}(${this.guild.id}) song manager. Key: ${key}`
        );
        (this.client as Rawon | undefined)?.queueState.requestSave();
        return super.set(key, data);
    }

    public delete(key: Snowflake): boolean {
        (this.client as Rawon | undefined)?.debugLog.logData(
            "info",
            "SONG_MANAGER",
            `Value ${key} deleted from ${this.guild.name}(${this.guild.id}) song manager.`
        );
        (this.client as Rawon | undefined)?.queueState.requestSave();
        return super.delete(key);
    }

    public clear(): void {
        super.clear();
        (this.client as Rawon | undefined)?.queueState.requestSave();
    }

    public sortByIndex(): this {
        return this.sort((a, b) => a.index - b.index);
    }
}
