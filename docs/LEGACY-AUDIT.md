# Legacy and duplicate-function audit

Date: 8 November 2026. Baseline: `316bee1`, plus the in-progress inventory, frontend cache and audit-progress changes present during inspection. Line references identify the inspected source and may move as those changes finish.

## Scope and conclusion

Read-only review of all native modules, frontend service callers and helpers, HTTP/MCP adapters, package scripts and test inventory. No app restart, live scan control, live database/journal write, installation, Rust build, commit or publication was performed. This report is the only repository edit from this audit.

There is **no evidence that the directory inventory can simply be deleted in favor of the checksum scanner**. Both are active, with different persisted contracts and callers. Most similarly named functions are adapters or deliberately different safety operations. The largest remaining risks are repeated expensive read work and contract drift, not a second completely abandoned application.

Static reference analysis does **not** prove deletion is safe. Rust macros, tests, IPC registration, string-dispatched RPC, persisted journals and external MCP consumers all matter. No broad cleanup is authorized by this report.

## Findings, in priority order

### 1. Active folder reads still perform unpaginated derived-state work

**Classification:** live performance debt, not dead code. **Priority:** high for large directories; magnitude needs measurement.

`src-tauri/src/main.rs:344-398` (`list_folder_service`) walks every matching sibling via `Index::each_entry`, computes `effective_cached`, builds counts, and only then limits the returned entries to 100. `main.rs:257-272` calls `rollup::derive`; `src-tauri/src/rollup.rs:61-103,109-133` can enumerate descendants for an unmarked folder with marked descendants. Its 2,000-entry/64-depth bounds are per derivation, not per complete listing. The work is selective, not performed for every folder, but pagination does not bound the total filesystem work on a cache miss.

Callers: desktop `list_folder`, HTTP `list_folder` (`gateway.rs:800` onward), MCP `read_folder` (`scripts/mcp-server.mjs:99`), main/preview folder views. `next_attention_service` (`main.rs:1167-1219`) also repeatedly calls the non-shared-cache `effective` wrapper while traversing entries/directories. `effective` creates a fresh confirmation cache each time; folder-list reads correctly share one cache across their operation.

**Safe next step:** instrument synthetic wide directories with marked subtrees; count native enumerations and confirmation lookups, separate cold/warm reads, filter changes, and next-attention. Consider one operation-scoped confirmation cache for next-attention and reusable derived summaries with explicit freshness fences. Do not reuse stale rollups as copy authority. The session cache reduces repeated requests but does not make an expensive miss cheap.

### 2. Keyboard first-attention bypasses the new shared listing cache

**Classification:** parallel live frontend read path. **Priority:** medium.

`src/App.tsx:545-565` (`firstAttention`) invokes `list_folder` directly for attention, may refresh an unobserved folder, then may read all entries as a fallback. The normal main-column effect and `FolderColumns` now use `FolderReadCache` (`src/folder-read-cache.ts:8-35`). This leaves repeated keyboard navigation outside the new request/observation reuse contract.

This path intentionally ignores the current search and targets the first eligible row; it is not a duplicate of the global `next_attention` service, which searches more broadly and wraps around. Do not substitute one for the other without preserving navigation semantics.

**Safe next step:** route the same query through the shared cache, preserving force-refresh, context cancellation, mixed-rollup exclusion and fallback behavior. Add a caller-level regression, not only an isolated cache test. Inspect current cache edits before implementing, since their API changed during this audit.

### 3. Three active filesystem consumers duplicate enumeration, but are not interchangeable

**Classification:** architectural duplication with distinct contracts. **Priority:** medium, especially simultaneous jobs.

- `src-tauri/src/index.rs:126` (`refresh`) and `:449` (`start`) maintain directory summaries, navigation listing generations and the durable metadata queue. The current work introduces bounded parallel inventory. Its persisted `directories`, `directory_children`, `listing_entries`, `listing_heads`, `queue` and `meta` tables are consumed directly.
- `src-tauri/src/audit.rs:311-322,382,464,702` discovers source allocation and then hashes eligible file contents, with durable run/leaf evidence and watcher invalidation.
- `src-tauri/src/copy_queue.rs:534-625` plans selected copy paths afresh and `:656` onward verifies and copies under stronger source/destination constraints.

Assistance directly reads the inventory schema (`assistance.rs:76` onward), verifies matching completed scope and freshness (`:612-675`), then caches its tree by scope/completion stamp. Folder listing, known-directory search and next-attention also require Index. Audit completion alone does not satisfy those contracts. Copy planning must recheck source state rather than trusting either scanner's old observations.

**Safe next step:** measure concurrent inventory/audit contention on a synthetic corpus. If consolidation is desired later, design a shared observation event/adapter and migration contract first. Do not point Assistance at audit counters or remove inventory controls. Separate logical length, allocated bytes, metadata coverage, source hashes and verified copy evidence throughout.

### 4. HTTP unknown-method error is an obsolete capability statement

**Classification:** confirmed stale text. **Priority:** low, but misleading to agents.

`src-tauri/src/gateway.rs:857` (unknown-method arm) says: “Recovery and filesystem execution are not available through MCP.” The same dispatcher supports `recover_review` and `control_copy_queue`; `scripts/mcp-server.mjs:50-63` registers those tools, with fencing and explicit confirmation requirements.

**Safe next step:** replace the error with a neutral unknown-method error or direct callers to advertised capabilities. Add a dispatch assertion that the error does not contradict supported operations. This is not evidence that the recovery/copy services themselves are obsolete.

### 5. Small test-only wrappers and stale frontend declarations

**Classification:** cleanup candidates, not proven safe deletions. **Priority:** low.

- `Audit::is_running`, `src-tauri/src/audit.rs:193`, is referenced by the native test wait helper (`:1445`) but no production caller was found.
- `CopyQueue::start`, `src-tauri/src/copy_queue.rs:255-265`, generates a job ID and forwards to `start_with_job`. Its visible call sites are copy-queue tests; production `copy_control` uses the durable-fenced launch path. Consider `#[cfg(test)]` for the convenience wrapper after verifying all build targets.
- `Index::flush_chunk` (`index.rs:278`) and `CopyQueue::hold_next_worker` (`copy_queue.rs:212`) are already explicitly test-only. They are not abandoned production paths.
- A strict TypeScript inspection (`npx --no-install tsc --noEmit --noUnusedLocals --noUnusedParameters`) found unused App icons `ArrowDownToLine`, `Clock3`, `Download`, `ExternalLink`, `CircleHelp`, `Trash2`; unused App types `Decision`, `Effective`; unused App `pageCount` (approximately line 994); and unused default React import in `src/main.tsx:1`.

The strict compiler invocation also encountered missing `AuditStatus` fields for checkpoint counters while parallel edits were in progress. That is an intermediate snapshot, **not** a final-build verdict. Rerun type checking once the source settles. The strict optional unused flags are not the project's normal build configuration.

## Duplication that should be preserved or consolidated only with contracts

### IPC, HTTP and MCP wrappers

`main.rs` Tauri commands delegate through blocking tasks to services; `gateway.rs:777-858` dispatches to the same services. Examples: `list_folder_service`, `details_service`, `scan_service`, audit services, `copy_control::control`, `recovery_control::control` and `assistance::snapshot`. `scripts/mcp-server.mjs:25` is a schema/parameter adapter through `GatewayClient`, not a separate migration implementation.

HTTP adds instance fencing, provenance and bounded response projections. `gateway.rs:551` projects details from the shared service; it is intentionally not identical to the desktop response. `scripts/gateway-client.mjs:26-87` rereads the local descriptor, verifies instance identity and refuses automatic mutation retries. Do not remove these checks as duplicate validation. The mutating-method lists in the client and gateway are repeated contracts worth checking together whenever a new operation is added.

### Persisted legacy compatibility

- `main.rs:664-672` refuses replay of old operation IDs without matching durable payload receipts. This is a safety guard, not dispensable legacy handling.
- `main.rs:457` keeps old history without stable keys from being attributed to a replacement disk. Do not guess identity from drive letters.
- `review.rs:418-477` permits legacy surface reports only for reveal planning (`allow_legacy_report=true`), while durable operations use the stricter branch. Current `ReviewActivity.ts` reports `following`; a future removal needs explicit compatibility/version analysis.
- `store.rs:656` tests optional preview fields for old documents. `model.ts` parsing defaults and `tests/model.test.ts:248` preserve those documents in the frontend too.
- `on-external` remains a supported manual assertion, distinct from `manual-confirmed`. Both appear in the status schema, UI labels, native validation, Assistance and MCP. Removing the older name without a journal migration would lose meaning; neither proves verified backup coverage.

### Path, exclusion, fingerprint and guard helpers

There are related implementations in `filesystem.rs`, `audit.rs`, `copy_queue.rs`, `watcher.rs`, `review.rs`, `assistance.rs` and `model.ts`. Not all have identical contracts:

- `filesystem::safe` (`filesystem.rs:147`) validates then drops ancestor handles. `guard_path` (`:152`) returns held handles. Copy guards (`copy_queue.rs:1032-1210`) add physical-volume validation, destination creation and handle-sharing rules. These are not interchangeable safety primitives.
- `filesystem::inspect` (`:196`) produces a navigation metadata fingerprint. Audit stamps/handle identities (`audit.rs:1199` onward) and copy stamps (`copy_queue.rs:1267` onward) support different evidence. None should be substituted for SHA-256 content verification.
- Audit exclusion matching (`audit.rs:1188`), copy exclusions (`copy_queue.rs:915`) and watcher exclusions (`watcher.rs:173`) share exact-subtree/trailing-star concepts. Watcher rules are normalized and validated up front. A shared matcher may reduce drift, but first add case, sibling-prefix, trailing-star, root and invalid-rule vectors for all three.
- Frontend path helpers serve display/navigation; native normalization and volume checks remain authoritative. Cross-language repetition here is not reason to remove native validation.

## Validation performed and remaining gates

Performed: source inventory; direct service/IPC/RPC/MCP caller tracing; production-versus-test reference searches; strict no-emit TypeScript inspection; inspection of tests and persisted schema dependencies. No Rust compilation or runtime scan was performed by this audit. Existing documented test success is historical evidence, not a new test run.

Inspected coverage includes native index/audit/copy/recovery/gateway/session/watcher/journal tests, plus frontend mutation retention, old-document parsing, audit freshness, overflow, bridge authentication, inventory progress and new read-cache tests. Tests prove their exercised contracts, not arbitrary legacy removability.

Recommended gates before any follow-up cleanup:

1. Finish the current performance/checkpoint edits and rerun ordinary TypeScript build and all frontend/bridge tests.
2. Run native tests when the shared Rust target is available; keep synthetic source/destination/journal/runtime paths isolated.
3. Add cold/warm directory and keyboard-navigation benchmarks, plus operation-scoped next-attention tests.
4. Exercise fresh MCP tool discovery and parity, including unknown-method errors, without mutating the real migration journal.
5. Treat each cleanup as a separate small reviewed change. Preserve old-document, replay, volume-identity, no-replace, confirmation, pause/resume and freshness tests. A lack of static callers is a lead for investigation, never authorization or proof for deletion.
