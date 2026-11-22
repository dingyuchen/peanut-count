import { randomBytes } from 'node:crypto';
export async function getRandomBytesAsync(count) { return new Uint8Array(randomBytes(count)); }
