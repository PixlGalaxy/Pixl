import nodePath from "node:path";
import { fileURLToPath } from "node:url";
import { includeIgnoreFile } from "@eslint/compat";
import { common, modules, node, prettier, typescript, extend, ignores } from "@stegripe/eslint-config";

const gitIgnore = nodePath.resolve(fileURLToPath(import.meta.url), "..", ".gitignore");

export default [...common, ...modules, ...node, ...prettier, ...extend(typescript, [{
    rule: "typescript/no-unnecessary-condition",
    option: ["off"]
}], ...ignores), includeIgnoreFile(gitIgnore), {
    ignores: [
        "yt-dlp-utils/*",
        "play-dl-importer/*",
        "play-dl-fix/*"
    ]
}, {
    rules: {
        "tsdoc/syntax": "off",
        "typescript/no-unsafe-assignment": "off",
        "typescript/no-unsafe-member-access": "off",
        "typescript/no-unsafe-call": "off",
        "typescript/no-unsafe-argument": "off",
        "typescript/no-unsafe-return": "off"
    }
}];
