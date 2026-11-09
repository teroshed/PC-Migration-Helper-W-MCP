import { createHash } from 'node:crypto';

export const OPENROUTER_MODEL = 'google/gemini-3.1-flash-lite';
export const OPENROUTER_ROUTE = 'openrouter/google-ai-studio';
export const OPENROUTER_ENDPOINT = 'https://openrouter.ai/api/v1/chat/completions';
export const OPENROUTER_PROVIDER = 'google-ai-studio';
const PROVIDER_NAME = 'Google AI Studio';
const ENV_KEY = 'PC_MIGRATION_BENCHMARK_OPENROUTER_KEY';
const SETTINGS = Object.freeze({
  contextMode: 'stateless', temperature: 0, seed: 42, reasoningEffort: 'minimal',
  revision: 'openrouter-google-ai-studio-standard-v1',
});
const ROLE_LIMITS = Object.freeze({
  owner: Object.freeze({ inputTokens: 3072, outputTokens: 2048, costMicros: 4000,
    timeoutMs: 30_000, inputBytes: 2816, responseBytes: 8192 }),
  policy: Object.freeze({ inputTokens: 6144, outputTokens: 2048, costMicros: 4800,
    timeoutMs: 30_000, inputBytes: 5888, responseBytes: 8192 }),
});
const ROLE_CALLS = Object.freeze({ owner: 168, policy: 56 });
const FRAMING_BYTE_ALLOWANCE = 256;
const MAX_ENVELOPE_BYTES = 64 * 1024;

function canonical(value) {
  if (Array.isArray(value)) return '[' + value.map(canonical).join(',') + ']';
  if (value !== null && typeof value === 'object') {
    return '{' + Object.keys(value).sort().map(key => JSON.stringify(key) + ':' + canonical(value[key])).join(',') + '}';
  }
  return JSON.stringify(value);
}

function exactKeys(value, keys) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    && Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}

function routeFor(role) {
  return { kind: 'metered-provider', model: OPENROUTER_MODEL, route: OPENROUTER_ROUTE,
    settings: structuredClone(SETTINGS), limits: structuredClone(ROLE_LIMITS[role]) };
}

/** A configuration proposal. The CLI still requires an independently authorized frozen matrix. */
export function proposedOpenRouterConfig(trialId = 'openrouter-google-ai-studio-first-heldout') {
  return {
    trialId,
    routes: { policy: routeFor('policy'), owner: routeFor('owner') },
    budget: { attempts: 224, inputTokens: 860_160, outputTokens: 458_752, costMicros: 940_800 },
  };
}

function validateRequest(request, role) {
  const fields = ['attemptId', 'cellId', 'role', 'questionNumber', 'model', 'route', 'configHash',
    'reserved', 'settings', 'limits', 'messages', 'signal'];
  if (!exactKeys(request, fields) || request.role !== role
      || request.model !== OPENROUTER_MODEL || request.route !== OPENROUTER_ROUTE
      || !/^[0-9a-f]{64}$/.test(request.attemptId) || !/^[0-9a-f]{64}$/.test(request.cellId)
      || !Number.isInteger(request.questionNumber) || request.questionNumber < 1 || request.questionNumber > 8
      || !(request.signal instanceof AbortSignal)) throw new Error('Request is outside the approved OpenRouter route.');
  const approved = routeFor(role);
  const configHash = createHash('sha256').update(canonical(approved)).digest('hex');
  if (request.configHash !== configHash || canonical(request.settings) !== canonical(approved.settings)
      || canonical(request.limits) !== canonical(approved.limits)) {
    throw new Error('OpenRouter request settings differ from the pinned configuration.');
  }
  const { inputTokens, outputTokens, costMicros, inputBytes } = approved.limits;
  if (canonical(request.reserved) !== canonical({ inputTokens, outputTokens, costMicros })) {
    throw new Error('OpenRouter request is missing its exact attempt reservation.');
  }
  if (!Array.isArray(request.messages) || request.messages.length !== 2
      || request.messages.some((message, index) => !exactKeys(message, ['role', 'content'])
        || message.role !== ['system', 'user'][index] || typeof message.content !== 'string'
        || !message.content.trim())) throw new Error('The benchmark requires two isolated text messages.');
  const bytes = Buffer.byteLength(JSON.stringify(request.messages), 'utf8');
  if (bytes > inputBytes || bytes + FRAMING_BYTE_ALLOWANCE > inputTokens) {
    throw new Error('The prompt exceeds its frozen byte and input-token proxy allowance.');
  }
}

function requestBody(request) {
  return {
    model: OPENROUTER_MODEL,
    messages: request.messages,
    provider: {
      only: [OPENROUTER_PROVIDER], order: [OPENROUTER_PROVIDER], allow_fallbacks: false,
      require_parameters: true, max_price: { prompt: 0.25, completion: 1.5, request: 0 },
    },
    service_tier: 'default',
    temperature: SETTINGS.temperature,
    seed: SETTINGS.seed,
    reasoning_effort: SETTINGS.reasoningEffort,
    max_tokens: request.limits.outputTokens,
    response_format: { type: 'json_object' },
    stream: false,
  };
}

async function boundedJson(response) {
  const contentType = response.headers?.get('content-type')?.split(';')[0].trim().toLowerCase();
  const length = response.headers?.get('content-length');
  if (contentType !== 'application/json' || (length !== null && length !== undefined
      && (!/^[0-9]+$/.test(length) || Number(length) > MAX_ENVELOPE_BYTES))
      || typeof response.body?.getReader !== 'function') {
    await response.body?.cancel().catch(() => {});
    throw new Error('OpenRouter response envelope was missing or oversized.');
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8', { fatal: true });
  let bytes = 0;
  let text = '';
  let finished = false;
  try {
    for (;;) {
      const next = await reader.read();
      if (next.done) break;
      if (!(next.value instanceof Uint8Array)) throw new Error('Invalid response stream.');
      bytes += next.value.byteLength;
      if (bytes > MAX_ENVELOPE_BYTES) throw new Error('Response envelope exceeded its byte cap.');
      text += decoder.decode(next.value, { stream: true });
    }
    finished = true;
    const raw = JSON.parse(text + decoder.decode());
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) throw new Error('Invalid response object.');
    return raw;
  } catch {
    throw new Error('OpenRouter returned an unverified or oversized JSON response.');
  } finally {
    if (!finished) void reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

function responseReceipt(raw, request) {
  const meta = raw.openrouter_metadata;
  const available = meta?.endpoints?.available;
  const selected = Array.isArray(available) ? available.filter(endpoint => endpoint?.selected === true) : [];
  if (raw.model !== OPENROUTER_MODEL || raw.object !== 'chat.completion'
      || !/^gen-[A-Za-z0-9_.-]{1,155}$/.test(raw.id ?? '')
      || (raw.provider !== undefined && raw.provider !== PROVIDER_NAME)
      || (raw.service_tier != null && raw.service_tier !== 'default')
      || meta?.requested !== OPENROUTER_MODEL || meta?.attempt !== 1 || meta?.is_byok !== false
      || selected.length !== 1 || selected[0].provider !== PROVIDER_NAME
      || selected[0].model !== OPENROUTER_MODEL) {
    throw new Error('OpenRouter did not verify the pinned model and provider on the first attempt.');
  }
  const usage = raw.usage;
  const inputTokens = usage?.prompt_tokens;
  const outputTokens = usage?.completion_tokens;
  const reportedCostUsd = usage?.cost;
  if (!Number.isSafeInteger(inputTokens) || inputTokens < 0
      || !Number.isSafeInteger(outputTokens) || outputTokens < 0
      || !Number.isSafeInteger(inputTokens + outputTokens) || usage.total_tokens !== inputTokens + outputTokens
      || !Number.isFinite(reportedCostUsd) || reportedCostUsd < 0
      || !Number.isSafeInteger(Math.ceil(reportedCostUsd * 1_000_000)) || usage.is_byok === true) {
    throw new Error('OpenRouter did not return verified token and cost accounting.');
  }
  const receipt = {
    model: OPENROUTER_MODEL, route: OPENROUTER_ROUTE, configHash: request.configHash,
    upstreamRequestId: raw.id,
    usage: { inputTokens, outputTokens, reportedCostUsd, costMicros: Math.ceil(reportedCostUsd * 1_000_000) },
  };
  if (inputTokens > request.limits.inputTokens || outputTokens > request.limits.outputTokens
      || receipt.usage.costMicros > request.limits.costMicros) return { ...receipt, status: 'failed' };
  const choice = Array.isArray(raw.choices) && raw.choices.length === 1 ? raw.choices[0] : null;
  const content = choice?.message?.content;
  if (raw.error != null || choice?.finish_reason !== 'stop' || choice.index !== 0
      || choice.message?.role !== 'assistant' || choice.message.tool_calls?.length
      || typeof content !== 'string' || Buffer.byteLength(content, 'utf8') > request.limits.responseBytes) {
    return { ...receipt, status: 'failed' };
  }
  try {
    const answer = JSON.parse(content);
    if (!answer || typeof answer !== 'object' || Array.isArray(answer)) return { ...receipt, status: 'failed' };
  } catch {
    return { ...receipt, status: 'failed' };
  }
  return { ...receipt, status: 'ok', body: content };
}

function buildTransports(fetchImpl, keyForDispatch) {
  const calls = { owner: 0, policy: 0 };
  const dispatched = new Set();
  const make = role => ({ kind: 'metered-provider', async invoke(request) {
    validateRequest(request, role);
    request.signal.throwIfAborted();
    if (dispatched.has(request.attemptId)) throw new Error('The exact attempt was already dispatched.');
    if (calls[role] >= ROLE_CALLS[role]) throw new Error('The pinned role request ceiling is exhausted.');
    const key = keyForDispatch();
    if (typeof key !== 'string' || !/^[!-~]{1,512}$/.test(key)) throw new Error('The dedicated benchmark credential is unavailable.');
    const signal = AbortSignal.any([request.signal, AbortSignal.timeout(request.limits.timeoutMs)]);
    calls[role]++;
    dispatched.add(request.attemptId);
    let response;
    try {
      response = await fetchImpl(OPENROUTER_ENDPOINT, {
        method: 'POST', redirect: 'error', signal,
        headers: { Authorization: 'Bearer ' + key, 'Content-Type': 'application/json',
          Accept: 'application/json', 'X-OpenRouter-Metadata': 'enabled' },
        body: JSON.stringify(requestBody(request)),
      });
    } catch {
      throw new Error('OpenRouter dispatch outcome is uncertain.');
    }
    if (response.url !== OPENROUTER_ENDPOINT || response.redirected || response.status !== 200) {
      await response.body?.cancel().catch(() => {});
      throw new Error('OpenRouter returned an unverified endpoint or unsuccessful status.');
    }
    return responseReceipt(await boundedJson(response), request);
  } });
  return { policy: make('policy'), owner: make('owner') };
}

/** CLI entry. Import and construction are effect-free. */
export function createTransports() {
  return buildTransports(globalThis.fetch, () => process.env[ENV_KEY]);
}

/** Injected fetch exercises the protocol with an invented credential and no environment access. */
export function createTestTransports(fetchImpl) {
  if (typeof fetchImpl !== 'function' || fetchImpl === globalThis.fetch) throw new Error('Supply an injected fake fetch.');
  return buildTransports(fetchImpl, () => 'synthetic-benchmark-key');
}
