# Question benchmark harness

This harness builds sanitized folder-shape fixtures, commits independent virtual owner intentions before the first question, and records each question and answer against that fixed truth. It is the early M5 harness for the production roadmap. The adaptive production interview and the complete live-model evaluation matrix need separate integration and execution.

The local snapshot, commitment, suite and replay commands use Node.js built-ins. All examples below use new output directories on E. An existing output directory is rejected, so a later run preserves earlier fixtures, commitments and traces.

```powershell
node scripts/question-benchmark.mjs suite --out E:\Temp\question-suite-001
node --test tests/question-benchmark.test.mjs
```

The suite contains invented uniform originals, confirmed rebuildable data, an unnamed original in a cache, local patches, mixed choices, existing child decisions, gaps and owner deferrals. It compares alphabetical, file-count, exception-first and confirm-or-keep policies. The wrong-owner-confirmation fault has a separate commitment and result. The suite reports `liveEvaluationExecuted: false`, and its live evaluation gate remains pending.

Use a provided metadata JSON file to prepare a structural dummy without scanning any directory. Entries use relative paths, kind `folder`, `file` or `gap`, and `logicalBytes` for files. A gap has reason `unreadable`, `link`, `depth-limit` or `unsupported`. Missing folder ancestors are implied. An optional category can be `ordinary`, `rebuild-candidate` or `protected-candidate`.

```json
{
  "entries": [
    { "path": "Example/Original.bin", "kind": "file", "logicalBytes": 128 },
    { "path": "Example/node_modules/generated.bin", "kind": "file", "logicalBytes": 4096 },
    { "path": "Example/Unavailable", "kind": "gap", "reason": "unreadable" }
  ]
}
```

```powershell
node scripts/question-benchmark.mjs snapshot --metadata E:\Temp\invented-metadata.json --out E:\Temp\question-fixture-001
```

Directory capture requires both an explicit source and `--allow-metadata-scan`. Keep the selected scope and results private until the sanitized export has been reviewed. This command reads directory entries and file metadata, including lengths. It never opens source file bodies. It writes surrogate names, the observed hierarchy, coarse category clues and size buckets. Source paths, personal names, notes, file contents, checksums, and raw operating-system errors are absent from the fixture.

```powershell
node scripts/question-benchmark.mjs snapshot --source-dir E:\ExampleScope --allow-metadata-scan --out E:\Temp\question-fixture-002 --max-entries 100000 --max-depth 64
```

Input and output must be absolute and disjoint in both directions. Existing linked or junction ancestors are rejected. Encountered links become gaps and their targets are skipped. Directory identities and membership timestamps are rechecked before publication. Capture aborts at its entry limit; a depth limit becomes an explicit unknown scope. Coarse file-type clues such as document, image or source come from an extension allowlist, without preserving the extension or source name. A supplied metadata fixture can use the corresponding `fileType` label. The Node traversal does not provide a filesystem snapshot or protection against an actively hostile process replacing paths between system calls. Use a stable authorized scope. Content changes during a metadata capture are outside its evidence.

Size buckets are lower powers of two, so their sums are synthetic approximate sizes. `--size-mode exact` deliberately retains observed logical lengths. Neither option measures allocated disk size. Every observed file has one file node. Inaccessible scopes have no invented file denominator. Large physical copies, hard-link identity, named streams, ACLs and restore behavior are outside this fixture format.

A physical dummy optionally recreates the sanitized hierarchy with new empty files. `--placeholder-bytes` permits up to 4096 fresh zero bytes per file, with a 64 MiB overall placeholder cap. Logical file sizes remain declared metadata. Gap nodes remain metadata and are not created as operating-system failures. The receipt distinguishes actual placeholder bytes from declared logical bytes.

```powershell
node scripts/question-benchmark.mjs materialize --fixture E:\Temp\question-fixture-001\fixture.json --out E:\Temp\question-physical-001 --max-files 10000
```

Assign the virtual owner's intentions independently, before asking a policy to generate any questions. `commit` accepts a separate author-controlled intent input. Assignments reference sanitized file node IDs. Supported statuses are `migrate-now`, `last-moment`, `redownload` and `erase`. Unspecified files receive a seeded virtual choice independent of filenames and category clues. A seed is used only while creating the oracle. It is never consulted during the interview. These choices describe a synthetic owner and cannot infer the migration owner's actual intent for undecided files.

```json
{
  "seed": 17,
  "keepProbability": 0.7,
  "unknownChoice": "keep",
  "assignments": [ { "nodeId": "n000002", "status": "migrate-now" } ],
  "knowledge": {
    "recognizedFiles": [ "n000002" ],
    "recognizedGroups": [],
    "mixedGroups": [],
    "verifiedRebuildGroups": [],
    "deferredGroups": [],
    "acceptsNameHints": false
  }
}
```

The IDs in this example must be checked against the generated fixture. An oracle must cover every observed file once. Its declared knowledge says which exact files or groups the simulated owner recognizes, which groups are mixed, which rebuild recipes the owner can attest to, and which work is deferred. Ordinary rebuild attestations cannot cover keep-intent files. Use `--fault-injection` to commit a deliberately incorrect owner attestation as a separately scored fault case.

```powershell
node scripts/question-benchmark.mjs commit --fixture E:\Temp\question-fixture-001\fixture.json --intent-input E:\Temp\independent-intent.json --out E:\Temp\question-oracle-001
node scripts/question-benchmark.mjs replay --fixture E:\Temp\question-fixture-001\fixture.json --oracle E:\Temp\question-oracle-001\oracle.json --commitment E:\Temp\question-oracle-001\commitment.json --policy confirm-or-keep --out E:\Temp\question-replay-001
```

The commitment hashes the sanitized fixture and full oracle. Replay verifies both before the first question and after each answer. Policies receive only public fixture metadata, eligible candidate IDs, and prior public answers. They never receive the oracle or its knowledge through the policy API. This separation protects against accidental leakage. It is not an adversarial sandbox for executable policy code running in the same process. Keep oracle files out of the questioning model's context and use an independent evaluator for live runs.

`createInterview()` provides `next()`, `apply()` and `status()` for an evaluator or future model adapter. One question remains stable until answered. Answers carry the question ID and target ID. Review children and Defer create ancestor barriers. Existing exact child decisions survive broader marks. Confirm-or-keep proposes retention when rebuildability is uncertain. A broad redownload answer requires `explicitRebuildConfirmation: true`, while a deliberate broad omission requires `explicitOmissionConfirmation: true`. An uncertain simulated owner can preserve redundantly or defer according to its precommitted knowledge. A negative mark here changes classification only.

Scores report incorrectly excluded keep-intent files and bytes, exact intent matches, redundant retained bytes, resolved files, deferred files, pending files and unknown scopes. The ranking order is preservation errors, coverage violations, unresolved known files, question count, redundant retained bytes and measured replay time. Rank only runs over the same fixture and oracle, and keep fault results separate. A pass can exhaust eligible questions while its overall status remains partial because gaps or deferrals remain. A defer-all strategy cannot win by reporting completion.

For held-out evaluation, an independent fixture owner should prepare a new scope with `--split held-out`, commit its oracle, and freeze hashes before selecting a policy. Declare each evaluation cell in advance using fixture hash, oracle hash, policy, exact model, exact route and trial ID. Keep development and held-out fixtures distinct. Freeze the evaluation matrix and stop rules before viewing held-out answers. Record every missing or failed cell, including provider failures and budget stops. A held-out label alone does not establish independence.

Actual AI answers can later be scored using `--responses` and `--evidence-kind recorded-ai-responses`, plus `--provenance` containing `model`, `route`, `trialId`, `authorizationRef`, SHA-256 `promptLogHash` and `responseLogHash`, and boolean `reviewed`. Optional `providerLatencyMs`, `costUsd`, `inputTokens` and `outputTokens` record measured provider use. Missing provider measurements remain absent. The response records must contain the exact question IDs. These fields are operator-supplied receipts requiring independent review. The harness does not authenticate their origin. Its declared-cell gate requires reviewed model/route/trial provenance and a complete recorded interview for every required cell. Human and scripted answers retain their separate evidence labels. JSON inputs are bounded to 64 MiB. Credentials and raw provider payloads are rejected as provenance fields.

The live driver adds an explicit transport interface. It performs a serial run with one adapter for the policy and one for the simulated owner. The policy prompt contains a bounded slate of sanitized candidates and prior decisions. The owner prompt contains the exact question and the limited facts that persona knew before the trial. An adaptive reply can propose a keep decision for one eligible candidate. The question ID binds its exact wording, and the owner reply is fenced to that ID. The template and conservative comparators use local questions and the same owner role. A failed adaptive call remains a failed cell.

The proposed corpus has six ordinary scenarios with three policies each and one separately scored false owner confirmation scenario with three policies. The scopes cover coherent originals, confirmed rebuildability, an unnamed original in a cache, local patches, mixed file decisions, and gaps and deferral. Eight questions per cell allow up to 168 owner attempts and 56 adaptive policy attempts. These are ceilings. The report lists every declared cell, including omitted or failed cells. Fixture invalidation during a reply has a separate simulated transport regression.

Create the private pack, bind exact routes and budgets, then provide a separately reviewed authorization receipt and adapter.

```powershell
node scripts/question-benchmark/live-cli.mjs precommit --out E:\Temp\question-live-pack-001
node scripts/question-benchmark/live-cli.mjs freeze --corpus E:\Temp\question-live-pack-001\corpus.private.json --config E:\Temp\question-live-config.json --out E:\Temp\question-live-frozen-001
node scripts/question-benchmark/live-cli.mjs run --corpus E:\Temp\question-live-pack-001\corpus.private.json --matrix E:\Temp\question-live-frozen-001\matrix.json --authorization E:\Temp\question-live-authorization.json --adapter E:\Temp\question-live-adapter.mjs --out E:\Temp\question-live-run-001
node scripts/question-benchmark/live-cli.mjs inspect --journal E:\Temp\question-live-run-001\attempts.jsonl
```

Use the exact model identifiers, route labels, settings and ceilings for both roles. The following example configures a local fake test. It illustrates the envelope, with zero direct cost and reserved token allowances of 2,048 input and 512 output tokens per attempt.

```json
{
  "trialId": "local-fake-example",
  "routes": {
    "policy": {
      "kind": "injected-fake",
      "model": "fixture-policy",
      "route": "fake:policy",
      "settings": { "contextMode": "stateless", "temperature": 0, "seed": 42 },
      "limits": {
        "inputTokens": 2048, "outputTokens": 512, "costMicros": 0,
        "timeoutMs": 30000, "inputBytes": 32768, "responseBytes": 8192
      }
    },
    "owner": {
      "kind": "injected-fake",
      "model": "fixture-owner",
      "route": "fake:owner",
      "settings": { "contextMode": "stateless", "temperature": 0, "seed": 42 },
      "limits": {
        "inputTokens": 2048, "outputTokens": 512, "costMicros": 0,
        "timeoutMs": 30000, "inputBytes": 32768, "responseBytes": 8192
      }
    }
  },
  "budget": { "attempts": 224, "inputTokens": 458752, "outputTokens": 114688, "costMicros": 0 }
}
```

The authorization JSON contains the frozen matrix's `manifestHash`, `kind`, an opaque `reference` to the actual authorization, and the trusted module's lowercase SHA-256 `adapterSha256`. The fake kind is `synthetic-test-only`. A live run requires `explicit-live-authorization` after the operator authorizes the exact routes, models, input scope, total cost and token/request ceilings. Authorize a cap of at most 168 times the owner attempt ceiling plus 56 times the policy attempt ceiling to reserve the full proposed matrix. A smaller cap can leave cells omitted. Prior review funding covers its own review.

The trusted standalone module exports `createTransports()` returning distinct `policy` and `owner` adapters. Each adapter has a `kind` matching the frozen route and an `invoke(request)` function. The request carries `attemptId`, `role`, `model`, `route`, `configHash`, `settings`, `limits`, `messages` and an abort `signal`. The adapter must enforce the exact configuration and token limits before dispatch. It returns `status` as `ok` or `failed`, the actual model and route, the configuration hash, `body` as one JSON string for a successful response, and any reported `usage` fields named `inputTokens`, `outputTokens` and `costMicros`. Preserve the actual provider request ID as `upstreamRequestId`. Missing usage remains unknown. The CLI executes the bytes whose hash was approved. Module initialization must be free of effects. Built-in Node imports are supported in this standalone module. Any external dependencies need their own review. The CLI includes neither a built-in route nor credential lookup.

The driver syncs each attempt reservation to the private journal before dispatch. Actual reported usage replaces its reservation when available. Uncertain failures, cancellation after dispatch and timeouts retain the reservation and stop further dispatch. An over-cap receipt retains the reported charge and stops the run. A later journal inspection lists unmatched reservations without replaying them. The private journal retains the prompts and replies. Review these before exporting evidence. A paused or interrupted trial requires reconciliation and fresh authorization before another trial.

A separately reviewed `codex-roleplay` route can record live role-play through isolated subagent prompts and host-reported model, effort, route and response identifiers. This route accounts for direct API charges only. Codex account limit windows are shared usage reports, with no per-attempt measured token or billed-cost receipt exposed by the available tools. Record those measurements as unknown. Its request cap is a session turn limit. A role-play coverage pass and the metered-provider qualification are separate gates.

The full provider gate requires completed cells, the selected policy's safety results, exact model and route receipts with measured usage, and independent review of the result and journal hashes, held-out independence and owner fidelity. The injected fake suite exercises transport and ledger behavior and cannot qualify as live evidence. Provider execution and the independent held-out review are pending.

The standalone `scripts/question-benchmark/openrouter-transport.mjs` adapter proposes `google/gemini-3.1-flash-lite` through OpenRouter / Google AI Studio, with the route label `openrouter/google-ai-studio` in the frozen matrix. It sends one non-streaming JSON request per authorized attempt to the pinned Chat Completions endpoint. Provider allowlisting, provider order, disabled fallbacks, required parameters and the default service tier pin the standard Google AI Studio endpoint. The request also sets maximum provider prices of $0.25 per million input tokens, $1.50 per million output tokens and $0 per request. [Model pricing](https://openrouter.ai/google/gemini-3.1-flash-lite/), [public endpoint metadata](https://openrouter.ai/api/v1/models/google/gemini-3.1-flash-lite/endpoints), [provider routing](https://openrouter.ai/docs/guides/routing/provider-selection), [service tiers](https://openrouter.ai/docs/guides/features/service-tiers)

The proposed settings are stateless context, temperature zero, seed 42 and minimal reasoning effort. The configuration hash binds those settings and each role's limits. The adapter verifies first-attempt routing metadata, the exact model and selected provider, and a non-BYOK receipt. It bounds the JSON response envelope and the assistant text separately. A verified receipt retains the OpenRouter generation ID, reported prompt and completion tokens, and `usage.cost` in USD-denominated credits. The journal stores that decimal as `reportedCostUsd` and rounds up to integer microdollars for budget accounting. [Chat Completions response metadata](https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion), [usage accounting](https://openrouter.ai/docs/cookbook/administration/usage-accounting), [credit denomination](https://openrouter.ai/support)

| Role | Maximum attempts | Input token allowance | Output token limit | Listed-price estimate per attempt | Reserved per attempt |
| --- | ---: | ---: | ---: | ---: | ---: |
| Owner | 168 | 3,072 | 2,048 | $0.003840 | $0.004000 |
| Adaptive policy | 56 | 6,144 | 2,048 | $0.004608 | $0.004800 |

At all 224 planned attempts, the listed-price estimate is $0.903168 and the local budget reservation is $0.940800. The input allowance uses serialized UTF-8 message bytes plus 256 bytes for framing as a local proxy. A saved fake journal contained 44 owner prompts with a 1,228-byte maximum and 15 policy prompts with a 3,119-byte maximum. Actual token counts and charges come from the provider receipt. The driver stops if reported usage exceeds the reservation. Provider billing and live receipt shape still require a verified run. [Usage accounting](https://openrouter.ai/docs/cookbook/administration/usage-accounting)

Generate the proposed route config with `proposedOpenRouterConfig()` and freeze it with the live CLI. After the operator authorizes the exact manifest and adapter hash, the adapter reads `PC_MIGRATION_BENCHMARK_OPENROUTER_KEY` inside its approved dispatch function. Use a dedicated benchmark key and an independently authorized credit limit. Test the transport locally with the following command.

```powershell
node --test tests/openrouter-transport.test.mjs tests/question-benchmark.test.mjs
```

The focused tests inject fake fetch responses, covering route pins, redirects, oversized or invalid JSON, usage projection and the request ceiling. The private preflight retains measured fake prompt sizes separately from its proposed token and cost reservations.
