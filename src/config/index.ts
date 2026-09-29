import path from "node:path";
import process from "node:process";
import type { ClientOptions, ShardingManagerMode } from "discord.js";
import { IntentsBitField, Options, Sweepers } from "discord.js";
import i18n from "i18n";
import { lang, enablePrefix, enableSlashCommand } from "./env.js";

// Intents configuration
const intents: number[] = [
    IntentsBitField.Flags.Guilds,
    IntentsBitField.Flags.GuildMessages,
    IntentsBitField.Flags.GuildExpressions,
    IntentsBitField.Flags.GuildVoiceStates,
    IntentsBitField.Flags.GuildModeration
];

// Check if enablePrefix is true and activate MessageContent
if (enablePrefix) {
    intents.push(IntentsBitField.Flags.MessageContent);
}

// Check if both enablePrefix and enableSlashCommand are false
if (!enablePrefix && !enableSlashCommand) {
    console.log("Both Slash Command and Prefix are disabled. Stopping the bot.");
    process.exit(1);
}

const botUserSweepFilter = (user: { bot: boolean; id: string; client: { user: { id: string } } }): boolean =>
    user.bot && user.id !== user.client.user.id;

// Define client options
export const clientOptions: ClientOptions = {
    allowedMentions: { parse: ["users"], repliedUser: true },
    intents,
    makeCache: Options.cacheWithLimits({
        ...Options.DefaultMakeCacheSettings,
        MessageManager: { maxSize: 50 },
        ThreadManager: { maxSize: 50 },
        PresenceManager: 0,
        ReactionManager: 0,
        ReactionUserManager: 0,
        GuildEmojiManager: 0,
        GuildStickerManager: 0,
        BaseGuildEmojiManager: 0,
        GuildScheduledEventManager: 0,
        GuildInviteManager: 0,
        AutoModerationRuleManager: 0,
        StageInstanceManager: 0
    }),
    sweepers: {
        ...Options.DefaultSweeperSettings,
        messages: {
            interval: 300,
            filter: Sweepers.filterByLifetime({ lifetime: 3_600 })
        },
        threads: {
            interval: 300,
            filter: Sweepers.filterByLifetime({
                lifetime: 3_600,
                getComparisonTimestamp: (el) => el.archiveTimestamp ?? 0,
                excludeFromSweep: (el) => el.archived !== true
            })
        },
        users: {
            interval: 3_600,
            filter: () => botUserSweepFilter
        }
    }
};

// i18n configuration
i18n.configure({
    defaultLocale: "en",
    directory: path.join(process.cwd(), "lang"),
    locales: ["en", "es", "id", "fr", "zh-CN", "zh-TW", "uk", "vi", "pt-BR", "ja", "tr"],
    objectNotation: true,
    retryInDefaultLocale: true,
    updateFiles: false
});

i18n.setLocale(lang);

export const shardsCount: number | "auto" = "auto";
export const shardingMode: ShardingManagerMode = "worker";
export * from "./env.js";

export { default } from "i18n";
