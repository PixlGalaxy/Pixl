/**
 * Maps `items` with at most `limit` promises in flight. Results keep the input order,
 * and a rejected item becomes `undefined` instead of failing the whole batch.
 */
export async function mapWithConcurrency<T, R>(
    items: readonly T[],
    limit: number,
    mapper: (item: T, index: number) => Promise<R>
): Promise<(R | undefined)[]> {
    const results: (R | undefined)[] = Array.from({ length: items.length });
    let cursor = 0;

    async function worker(): Promise<void> {
        while (cursor < items.length) {
            const index = cursor++;
            try {
                // eslint-disable-next-line no-await-in-loop
                results[index] = await mapper(items[index], index);
            } catch {
                results[index] = undefined;
            }
        }
    }

    await Promise.all(Array.from({ length: Math.min(limit, items.length) }, async () => worker()));
    return results;
}
