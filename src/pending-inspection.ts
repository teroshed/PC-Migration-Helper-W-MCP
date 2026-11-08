export interface PendingInspectable { id: string; operation: unknown; origin?: string }
export interface PendingSnapshot { writer: unknown; drafts: unknown; requests: readonly PendingInspectable[] }

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value)
    .sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(([key, item]) => [key, canonical(item)]));
  return value;
}
export function pendingJSON(value: unknown): string { return JSON.stringify(canonical(value)); }
export async function textHash(text: string): Promise<string> {
  return Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text))), n => n.toString(16).padStart(2, '0')).join('');
}
export async function pendingFingerprint(snapshot: PendingSnapshot): Promise<string> {
  return textHash(pendingJSON({ writer: snapshot.writer, drafts: snapshot.drafts }));
}
export function pendingMetadata(requests: readonly PendingInspectable[]): { total: number; entries: Record<string, unknown>[]; truncated: boolean } {
  return { total: requests.length, truncated: requests.length > 16, entries: requests.slice(0, 16).map(item => {
    const operation = item.operation && typeof item.operation === 'object' ? item.operation as Record<string, unknown> : {};
    const path = typeof operation.path === 'string' ? operation.path : '';
    return { id: item.id, type: operation.type ?? 'unknown', pathPreview: path.slice(0, 256), pathPreviewTruncated: path.length > 256 };
  }) };
}
export function textChunk(text: string, offset: number, limit: number): { text: string; totalBytes: number; nextOffset: number | null } {
  const bytes = new TextEncoder().encode(text);
  if (!Number.isSafeInteger(offset) || offset < 0 || offset > bytes.length || offset < bytes.length && (bytes[offset]! & 0xc0) === 0x80) throw new Error('Text offset must be a UTF-8 boundary inside this request.');
  if (!Number.isSafeInteger(limit) || limit < 4 || limit > 16_384) throw new Error('Text limit must be between 4 and 16384 UTF-8 bytes.');
  let end = Math.min(bytes.length, offset + limit);
  while (end > offset && end < bytes.length && (bytes[end]! & 0xc0) === 0x80) end -= 1;
  return { text: new TextDecoder('utf-8', { fatal: true }).decode(bytes.slice(offset, end)), totalBytes: bytes.length, nextOffset: end < bytes.length ? end : null };
}
export async function inspectPending(read: () => PendingSnapshot, command: Record<string, unknown>): Promise<Record<string, unknown>> {
  const snapshot = read();
  const fingerprint = await pendingFingerprint(snapshot);
  if (command.expectedPendingFingerprint !== fingerprint) throw new Error('The retained requests or drafts changed. Read the current pending fingerprint before continuing.');
  let result: Record<string, unknown>;
  const offset = Number(command.offset);
  const limit = Number(command.limit);
  if (command.action === 'listPending') {
    if (!Number.isSafeInteger(offset) || offset < 0 || offset > snapshot.requests.length || !Number.isSafeInteger(limit) || limit < 1 || limit > 32) throw new Error('Pending list requires an offset in range and a limit between 1 and 32.');
    const entries = await Promise.all(snapshot.requests.slice(offset, offset + limit).map(async item => {
      const text = pendingJSON(item);
      const operation = item.operation && typeof item.operation === 'object' ? item.operation as Record<string, unknown> : {};
      const path = typeof operation.path === 'string' ? operation.path : '';
      return { operationId: item.id, origin: item.origin ?? 'retained', type: operation.type ?? 'unknown',
        pathPreview: path.slice(0, 256), pathPreviewTruncated: path.length > 256,
        totalBytes: new TextEncoder().encode(text).length, sha256: await textHash(text) };
    }));
    const next = offset + entries.length;
    result = { entries, total: snapshot.requests.length, offset, nextOffset: next < snapshot.requests.length ? next : null, pendingFingerprint: fingerprint };
  } else if (command.action === 'readPendingText') {
    const item = snapshot.requests.find(item => item.id === command.operationId);
    if (!item) throw new Error('This retained request is no longer present. Read the pending list again.');
    const text = pendingJSON(item);
    result = { operationId: item.id, offset, ...textChunk(text, offset, limit), sha256: await textHash(text), pendingFingerprint: fingerprint, encoding: 'utf-8', format: 'json' };
  } else throw new Error('Unsupported pending inspection action.');
  if (fingerprint !== await pendingFingerprint(read())) throw new Error('Pending work changed during inspection. The partial result was rejected; read the current fingerprint again.');
  return result;
}
