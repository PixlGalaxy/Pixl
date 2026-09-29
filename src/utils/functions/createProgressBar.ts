export function createProgressBar(current: number, total: number, size = 12): string {
    const ratio = total > 0 ? Math.min(1, Math.max(0, current / total)) : 0;
    const pos = Math.min(size, Math.max(1, Math.ceil(ratio * size)));

    return `${"━".repeat(pos - 1)}●${"─".repeat(size - pos)}`;
}
