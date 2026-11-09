import assert from 'node:assert/strict';
import test from 'node:test';
import { digest } from '../scripts/question-benchmark/fixture.mjs';
import { runLiveMatrix } from '../scripts/question-benchmark/live-driver.mjs';
import { freezeLiveMatrix, heldOutCorpus } from '../scripts/question-benchmark/live-matrix.mjs';
import { createTestTransports, OPENROUTER_ENDPOINT, OPENROUTER_MODEL, OPENROUTER_PROVIDER,
  OPENROUTER_ROUTE, proposedOpenRouterConfig } from '../scripts/question-benchmark/openrouter-transport.mjs';

function requestFor(role = 'owner', sequence = 1) {
  const config = proposedOpenRouterConfig().routes[role];
  const { model, route, settings, limits } = config;
  return { role, model, route, settings, limits, configHash: digest(config),
    attemptId: digest({ role, sequence }), cellId: digest({ scenario: 'invented-fixture' }), questionNumber: 1,
    reserved: { inputTokens: limits.inputTokens, outputTokens: limits.outputTokens, costMicros: limits.costMicros },
    messages: [{ role: 'system', content: 'Synthetic local fixture.' }, { role: 'user', content: '{"nodeId":"n000001"}' }],
    signal: new AbortController().signal,
  };
}

function completion(overrides = {}) {
  return {
    id: 'gen-injected-fake-only', object: 'chat.completion', model: OPENROUTER_MODEL,
    provider: 'Google AI Studio', service_tier: 'default',
    openrouter_metadata: { requested: OPENROUTER_MODEL, attempt: 1, is_byok: false,
      endpoints: { available: [{ model: OPENROUTER_MODEL, provider: 'Google AI Studio', selected: true }] } },
    choices: [{ index: 0, finish_reason: 'stop', message: { role: 'assistant', content: '{"synthetic":true}' } }],
    usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5, cost: 0.00000125, is_byok: false },
    ...overrides,
  };
}

function response(value = completion(), {
  status = 200, url = OPENROUTER_ENDPOINT, redirected = false, contentType = 'application/json', raw = false, length,
} = {}) {
  const headers = { 'Content-Type': contentType };
  if (length !== undefined) headers['Content-Length'] = String(length);
  const result = new Response(raw ? value : JSON.stringify(value), { status, headers });
  Object.defineProperty(result, 'url', { value: url });
  Object.defineProperty(result, 'redirected', { value: redirected });
  return result;
}

test('injected fake confirms the exact OpenRouter pin and preserves observed decimal usage', async () => {
  const calls = [];
  const { owner } = createTestTransports(async (url, init) => { calls.push({ url, init }); return response(); });
  const request = requestFor();
  const receipt = await owner.invoke(request);
  assert.deepEqual(receipt, { status: 'ok', model: OPENROUTER_MODEL, route: OPENROUTER_ROUTE,
    configHash: request.configHash, upstreamRequestId: 'gen-injected-fake-only', body: '{"synthetic":true}',
    usage: { inputTokens: 3, outputTokens: 2, reportedCostUsd: 0.00000125, costMicros: 2 } });
  assert.equal(calls.length, 1);
  const [{ url, init }] = calls;
  assert.equal(url, OPENROUTER_ENDPOINT);
  assert.equal(init.method, 'POST');
  assert.equal(init.redirect, 'error');
  assert.equal(init.headers.Authorization, 'Bearer synthetic-benchmark-key');
  assert.equal(init.headers['X-OpenRouter-Metadata'], 'enabled');
  const body = JSON.parse(init.body);
  assert.equal(body.model, OPENROUTER_MODEL);
  assert.deepEqual(body.provider, { only: [OPENROUTER_PROVIDER], order: [OPENROUTER_PROVIDER],
    allow_fallbacks: false, require_parameters: true, max_price: { prompt: 0.25, completion: 1.5, request: 0 } });
  assert.equal(body.service_tier, 'default');
  assert.equal(body.reasoning_effort, 'minimal');
  assert.equal(body.max_tokens, 2048);
  assert.equal(body.seed, 42);
  assert.deepEqual(body.response_format, { type: 'json_object' });
  assert.equal(body.stream, false);
  assert.equal(body.usage, undefined);
  await assert.rejects(owner.invoke(request), /already dispatched/);
  assert.equal(calls.length, 1);
});

test('unapproved route, settings, envelope and pre-cancelled calls never reach fake fetch', async () => {
  let calls = 0;
  const { owner } = createTestTransports(async () => { calls++; return response(); });
  const request = requestFor();
  for (const changed of [
    { ...request, model: 'alternate/model' },
    { ...request, route: 'openrouter/google-vertex' },
    { ...request, endpoint: 'https://example.invalid' },
    { ...request, settings: { ...request.settings, reasoningEffort: 'high' } },
    { ...request, limits: { ...request.limits, outputTokens: 4096 } },
    { ...request, reserved: { ...request.reserved, costMicros: 0 } },
    { ...request, messages: [{ role: 'system', content: 'x' }, { role: 'user', content: 'x'.repeat(3000) }] },
    { ...request, messages: [{ role: 'system', content: 'x' }, { role: 'assistant', content: 'y' }] },
  ]) await assert.rejects(owner.invoke(changed));
  const controller = new AbortController();
  controller.abort();
  await assert.rejects(owner.invoke({ ...request, signal: controller.signal }));
  assert.equal(calls, 0);
  assert.throws(() => createTestTransports(globalThis.fetch), /injected fake fetch/);
});

test('a fake redirect, alternative provider, tier, model or BYOK receipt fails closed', async () => {
  const base = completion();
  const variants = [
    response(base, { status: 302 }),
    response(base, { url: 'https://other.invalid/api/v1/chat/completions' }),
    response(base, { redirected: true }),
    response(completion({ model: 'alternate/model' })),
    response(completion({ service_tier: 'priority' })),
    response(completion({ provider: 'Google Vertex' })),
    response(completion({ openrouter_metadata: { ...base.openrouter_metadata, is_byok: true } })),
    response(completion({ openrouter_metadata: { ...base.openrouter_metadata, attempt: 2 } })),
    response(completion({ openrouter_metadata: { ...base.openrouter_metadata, endpoints: { available: [
      { provider: 'Google Vertex', model: OPENROUTER_MODEL, selected: true },
    ] } } })),
  ];
  for (const invalid of variants) {
    let calls = 0;
    const { owner } = createTestTransports(async () => { calls++; return invalid; });
    await assert.rejects(owner.invoke(requestFor()), /unverified endpoint|pinned model and provider/);
    assert.equal(calls, 1);
  }
});

test('fake response size, JSON, UTF-8 and measured usage are bounded and validated', async () => {
  const variants = [
    response(completion(), { contentType: 'text/html' }),
    response(completion(), { length: 65537 }),
    response('x'.repeat(65537), { raw: true }),
    response(new Uint8Array([255, 254]), { raw: true }),
    response('{broken', { raw: true }),
    response(completion({ usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 } })),
    response(completion({ usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 4, cost: 0.00000125 } })),
  ];
  for (const invalid of variants) {
    let calls = 0;
    const { owner } = createTestTransports(async () => { calls++; return invalid; });
    await assert.rejects(owner.invoke(requestFor()), /envelope|unverified or oversized JSON|token and cost accounting/);
    assert.equal(calls, 1);
  }
});

test('a verified failed answer or over-limit receipt preserves cost without forwarding model text', async () => {
  const modelText = content => [{ index: 0, finish_reason: 'stop', message: { role: 'assistant', content } }];
  for (const content of ['not JSON', '[]', JSON.stringify({ padded: 'x'.repeat(8192) })]) {
    const { owner } = createTestTransports(async () => response(completion({ choices: modelText(content) })));
    const receipt = await owner.invoke(requestFor());
    assert.equal(receipt.status, 'failed');
    assert.equal(receipt.body, undefined);
    assert.equal(receipt.usage.reportedCostUsd, 0.00000125);
  }
  const { owner } = createTestTransports(async () => response(completion({
    usage: { prompt_tokens: 3, completion_tokens: 2049, total_tokens: 2052, cost: 0.00425 },
  })));
  const receipt = await owner.invoke(requestFor());
  assert.equal(receipt.status, 'failed');
  assert.equal(receipt.usage.outputTokens, 2049);
  assert.equal(receipt.usage.reportedCostUsd, 0.00425);
});

test('the proposed matrix fits 224 attempts and a sub-dollar reservation with isolated role ceilings', async () => {
  const config = proposedOpenRouterConfig();
  const frozen = freezeLiveMatrix(heldOutCorpus(), config);
  assert.equal(frozen.manifest.plannedAttemptCeiling, 224);
  assert.equal(frozen.manifest.budget.costMicros,
    168 * config.routes.owner.limits.costMicros + 56 * config.routes.policy.limits.costMicros);
  assert.equal(frozen.manifest.budget.costMicros, 940_800);
  let calls = 0;
  const { policy, owner } = createTestTransports(async () => { calls++; return response(); });
  for (let index = 1; index <= 56; index++) await policy.invoke(requestFor('policy', index));
  await assert.rejects(policy.invoke(requestFor('policy', 57)), /request ceiling/);
  await owner.invoke(requestFor('owner'));
  assert.equal(calls, 57);
});

test('the benchmark journal keeps the fake observed decimal separate from its rounded reserve', async () => {
  const corpus = heldOutCorpus().slice(0, 1);
  const config = proposedOpenRouterConfig('synthetic-usage-projection-test');
  config.budget.attempts = 1;
  for (const role of ['policy', 'owner']) {
    config.routes[role].kind = 'injected-fake';
    config.routes[role].model = 'fixture-cost-projection';
    config.routes[role].route = 'fake:cost-projection';
  }
  const frozen = freezeLiveMatrix(corpus, config);
  const events = [];
  const adapter = role => ({ kind: 'injected-fake', async invoke(request) {
    assert.equal(role, 'owner');
    const { question } = JSON.parse(request.messages[1].content);
    return { model: request.model, route: request.route, configHash: request.configHash, status: 'ok',
      body: JSON.stringify({ questionId: question.questionId, nodeId: question.nodeId, action: 'answer', status: 'migrate-now' }),
      usage: { inputTokens: 3, outputTokens: 2, costMicros: 2, reportedCostUsd: 0.00000125 } };
  } });
  const result = await runLiveMatrix(frozen, corpus, {
    authorization: { manifestHash: frozen.manifestHash, kind: 'synthetic-test-only', reference: 'fake-receipt-projection' },
    transports: { policy: adapter('policy'), owner: adapter('owner') },
    journal: { record: event => events.push(event) },
  });
  assert.equal(result.liveEvaluationPass, false);
  assert.equal(result.rows[0].evidenceKind, 'simulated-transport');
  assert.equal(result.attempts[0].usage.reportedCostUsd, 0.00000125);
  assert.equal(result.attempts[0].accountedUsage.costMicros, 2);
  assert.equal(events.find(event => event.event === 'attempt-finished').usage.reportedCostUsd, 0.00000125);
});
