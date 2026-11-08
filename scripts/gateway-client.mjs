import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

export const defaultGatewayFile = fileURLToPath(new URL('../runtime/mcp-gateway.json', import.meta.url));
const MAX_RESPONSE_BYTES = 8 * 1024 * 1024;

export function validateDescriptor(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('Invalid native gateway descriptor. Reopen PC Migration Helper.');
  const endpoint = new URL(value.endpoint);
  if (endpoint.protocol !== 'http:' || endpoint.hostname !== '127.0.0.1' || !endpoint.port ||
      endpoint.pathname !== '/rpc' || endpoint.search || endpoint.hash || endpoint.username || endpoint.password) {
    throw new Error('The gateway must be a private IPv4 loopback endpoint at /rpc.');
  }
  if (value.schemaVersion !== 1 || typeof value.token !== 'string' || !/^[a-f0-9]{64}$/i.test(value.token) ||
      typeof value.instanceId !== 'string' || value.instanceId.length < 16 ||
      typeof value.pid !== 'number' || !Number.isSafeInteger(value.pid) || value.pid < 1) {
    throw new Error('Invalid native gateway identity. Reopen PC Migration Helper.');
  }
  return { endpoint: endpoint.href, token: value.token, instanceId: value.instanceId };
}

export class GatewayClient {
  constructor(descriptorFile = defaultGatewayFile) { this.descriptorFile = descriptorFile; }

  async call(method, params = {}) {
    let descriptor;
    try {
      const bytes = await readFile(this.descriptorFile);
      if (bytes.byteLength > 16_384) throw new Error('oversized descriptor');
      descriptor = validateDescriptor(JSON.parse(bytes.toString('utf8')));
    } catch {
      throw new Error('PC Migration Helper is not ready. Open the updated native app and check its MCP status.');
    }
    if (['mutate', 'scan', 'audit_control', 'audit_read_notifications', 'control_copy_queue', 'control_ui', 'recover_review'].includes(method)) {
      if (params.expectedInstanceId !== descriptor.instanceId) {
        throw new Error('The app session changed or was not supplied. Read get_review_state again and review the intended edit before using its new gateway.instanceId. No write was sent.');
      }
    }
    let response;
    try {
      response = await fetch(descriptor.endpoint, {
        method: 'POST', redirect: 'error', signal: AbortSignal.timeout(45_000),
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${descriptor.token}`,
          'X-Migration-Instance': descriptor.instanceId,
        },
        body: JSON.stringify({ method, params }),
      });
    } catch {
      // Mutations are never retried automatically after an uncertain acknowledgement.
      throw new Error('The native app did not acknowledge the request. Check that it is open. For a write, inspect the current revision before retrying the same operation ID.');
    }
    if (!response.ok) {
      await response.body?.cancel();
      throw new Error(`Native gateway rejected the request (HTTP ${response.status}). Reopen the app if its connection identity changed.`);
    }
    const reader = response.body?.getReader();
    if (!reader) throw new Error('The native gateway returned an empty response.');
    const chunks = [];
    let bytesRead = 0;
    while (true) {
      let packet;
      try { packet = await reader.read(); }
      catch { throw new Error('The native response was interrupted. Inspect the saved revision before retrying the same write ID; the operation was not automatically replayed.'); }
      const { done, value } = packet;
      if (done) break;
      bytesRead += value.byteLength;
      if (bytesRead > MAX_RESPONSE_BYTES) { await reader.cancel(); throw new Error('Native response exceeded 8 MB. Use a narrower path or a paginated tool.'); }
      chunks.push(value);
    }
    let payload;
    try { payload = JSON.parse(Buffer.concat(chunks).toString('utf8')); }
    catch { throw new Error('The native gateway returned invalid JSON. No mutation was replayed.'); }
    if (!payload || typeof payload !== 'object' || Array.isArray(payload) || typeof payload.ok !== 'boolean') {
      throw new Error('The native gateway returned an unsupported response.');
    }
    if (!payload.ok) throw new Error(typeof payload.error === 'string' ? payload.error.slice(0, 4000) : 'The native operation failed.');
    return payload.result ?? null;
  }
}
