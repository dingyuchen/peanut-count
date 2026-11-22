const items = new Map();
// Every write, in order, so tests can check how often a key was created.
export const writes = [];
export const AFTER_FIRST_UNLOCK = 0;
export async function getItemAsync(key) { return items.get(key) ?? null; }
export async function setItemAsync(key, value) { writes.push(key); items.set(key, value); }
export async function deleteItemAsync(key) { items.delete(key); }
