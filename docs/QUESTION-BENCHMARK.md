# Question benchmark harness

This harness builds sanitized folder-shape fixtures, commits independent virtual owner intentions before the first question, and records each question and answer against that fixed truth. It is the early M5 harness for the production roadmap. The adaptive production interview and the complete live-model evaluation matrix need separate integration and execution.

The harness uses Node.js built-ins and makes no network or provider calls. All examples below use new output directories on E. An existing output directory is rejected, so a later run preserves earlier fixtures, commitments and traces.

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

Assign the virtual owner's intentions independently, before asking a policy to generate any questions. `commit` accepts a separate author-controlled intent input. Assignments reference sanitized file node IDs. Supported statuses are `migrate-now`, `last-moment`, `redownload` and `erase`. Unspecified files receive a seeded virtual choice independent of filenames and category clues. A seed is used only while creating the oracle. It is never consulted during the interview. These choices describe a synthetic owner and cannot infer Daniel's actual intent for undecided files.

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

Before paid execution, Daniel must authorize the exact models/routes, sanitized input scope, concurrent request/token/spend caps and stop conditions. An adapter must reserve concurrent costs, retain failures and uncertain acknowledgements, and stop at its budget. None of these commands consumes provider credentials or performs live evaluation. Do not claim M5 complete until the declared live-answer matrix and held-out policy gates have actually passed.
