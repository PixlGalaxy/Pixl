/* eslint-disable unicorn/filename-case */
/* eslint-disable typescript/naming-convention */
import { spawn } from "node:child_process";
import process from "node:process";
import type { Readable } from "node:stream";
import ytdl, { exec } from "../../../yt-dlp-utils/index.js";
import { streamStrategy } from "../../config/index.js";
import type { Rawon } from "../../structures/Rawon.js";
import type { BasicYoutubeVideoInfo } from "../../typings/index.js";
import { checkQuery } from "./GeneralUtil.js";

type Unpromisify<T> = T extends Promise<infer U> ? U : T;

const { stream: pldlStream, video_basic_info } = await import("../../../play-dl-importer/index.js").then(x => x.default).catch(() => ({ stream: null, video_basic_info: null }));

export async function getStream(client: Rawon, url: string): Promise<Readable> {
    if (streamStrategy === "play-dl") {
        const isSoundcloudUrl = checkQuery(url);
        if (isSoundcloudUrl.sourceType === "soundcloud") {
            return client.soundcloud.util.streamTrack(url);
        }
        const rawPlayDlStream = await pldlStream?.(url, { discordPlayerCompatibility: true });
        return rawPlayDlStream?.stream as unknown as Readable;
    }

    return new Promise<Readable>((resolve, reject) => {
        const proc = exec(
            url,
            {
                output: "-",
                quiet: true,
                format: "bestaudio",
                limitRate: "300K",
                noPlaylist: true
            },
            { stdio: ["ignore", "pipe", "ignore"] }
        );

        const stdout = proc.stdout;
        if (!stdout) {
            reject(new Error("Error obtaining stdout from process."));
            return;
        }

        let exited = false;
        proc.once("exit", () => {
            exited = true;
        });

        const kill = (): void => {
            if (exited) return;
            if (process.platform === "win32" && typeof proc.pid === "number") {
                spawn("taskkill", ["/pid", String(proc.pid), "/T", "/F"], { windowsHide: true, stdio: "ignore" });
            } else {
                proc.kill("SIGKILL");
            }
        };

        proc.once("error", err => {
            kill();
            reject(err);
        });

        stdout.once("error", err => {
            kill();
            reject(err);
        });

        stdout.once("close", kill);

        proc.once("spawn", () => {
            resolve(stdout);
        });
    });
}

export async function getInfo(url: string): Promise<BasicYoutubeVideoInfo> {
    if (streamStrategy === "play-dl") {
        const rawPlayDlVideoInfo = await video_basic_info?.(url) as unknown as Unpromisify<ReturnType<NonNullable<typeof video_basic_info>>>;
        return {
            duration: rawPlayDlVideoInfo.video_details.durationInSec,
            id: rawPlayDlVideoInfo.video_details.id ?? "",
            thumbnails: rawPlayDlVideoInfo.video_details.thumbnails,
            title: rawPlayDlVideoInfo.video_details.title ?? "",
            url: rawPlayDlVideoInfo.video_details.url
        };
    }
    return ytdl(url, {
        dumpJson: true
    });
}
