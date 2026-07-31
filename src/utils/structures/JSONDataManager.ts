/* eslint-disable unicorn/filename-case */
import { access, mkdir, readFile, rename, writeFile } from "node:fs/promises";
import path from "node:path";
import { OperationManager } from "./OperationManager.js";

export class JSONDataManager<T> {
    private readonly manager = new OperationManager();
    private _data: T | null = null;

    public constructor(public readonly fileDir: string, private readonly legacyFileDir?: string) {
        void this.init();
    }

    public get data(): T | null {
        return this._data;
    }

    public async save(data: () => T): Promise<T | null> {
        await this.manager.add(async () => {
            const dat = data();
            await mkdir(path.dirname(this.fileDir), { recursive: true });
            await writeFile(this.fileDir, JSON.stringify(dat));
        });

        return this.load();
    }

    private async init(): Promise<void> {
        await this.migrate();
        await this.load();
    }

    private async migrate(): Promise<void> {
        if ((this.legacyFileDir?.length ?? 0) === 0) return;
        const legacy = this.legacyFileDir ?? "";

        const legacyExists = await access(legacy).then(() => true).catch(() => false);
        const targetExists = await access(this.fileDir).then(() => true).catch(() => false);
        if (!legacyExists || targetExists) return;

        try {
            await this.manager.add(async () => {
                await mkdir(path.dirname(this.fileDir), { recursive: true });
                await rename(legacy, this.fileDir);
            });
        } catch {
            this._data = null;
        }
    }

    private async load(): Promise<T | null> {
        try {
            await this.manager.add(async () => {
                this._data = JSON.parse(await readFile(this.fileDir, "utf8")) as T;
            });

            return this._data;
        } catch {
            return this.data;
        }
    }
}
