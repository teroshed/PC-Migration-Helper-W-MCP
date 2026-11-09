# Desktop, HTTP and MCP coverage

The native app owns the review journal, volume guards, inventory, source audit,
copy coordinator and recovery service. The desktop and authenticated loopback
HTTP gateway use those operations. The stdio MCP bridge translates typed tool
inputs to the same HTTP operations. API version **0.4.0** advertises **29 tools**
on fresh initialization.

Window presentation has a separate acknowledged command channel. Inspect
`get_ui_state`, submit `control_ui`, and observe its operation receipt until
`applied` or `error`. Acceptance alone does not mean the window changed. A fresh
native-issued UI session prevents commands or reports from an old window from
being adopted by its replacement.

## Action inventory

HTTP names below are the `method` in `POST /rpc`. Journal changes use `mutate`
with the indicated operation. See [MCP.md](MCP.md) for authentication, schemas,
confirmation, fencing, retries and asynchronous status.

| Desktop workflow | HTTP operation | MCP equivalent |
| --- | --- | --- |
| Inspect drives, journal revision, configured roots and gateway state | `get_state` | `get_review_state` |
| Inspect the saved review position and actual rendered columns/selection | `get_review_view` | `get_review_view` |
| Open a drive, recent location, parent, breadcrumb, typed path or folder | `mutate`: review `openMain` | `navigate_review` / `control_review_view` |
| Select or deselect the current row | `mutate`: review `select` | `control_review_view` |
| Open nested folder previews; close a preview branch | `mutate`: review `openPreview` / `closePreview` | `control_review_view` |
| Search/filter a folder, change page or saved scroll positions | `mutate`: review `setView` | `control_review_view` |
| Find the next entry needing attention and reveal it | `next_attention`, then review navigation/selection | `next_needing_attention`, then `control_review_view` |
| Read a directory or refresh its metadata | `list_folder` | `read_folder` |
| Refresh the displayed directory / retry its listing | `control_ui`: `refreshVisible` | `control_ui` |
| Search indexed folder names and open a result | `search_dirs`, then review `openMain` | `search_folders`, then `navigate_review` |
| Show the current known-folder search in the window | `control_ui`: `searchKnownFolders` | `control_ui` |
| Apply any of the eight decisions from a row menu or decision picker | `mutate`: `classify` | `save_decision` |
| Save or clear a note; undo a decision/note | `mutate`: `note` / `undo` | `save_note` / `undo_review_change` |
| Record an agent proposal separately from user decisions | `mutate`: `suggest` | `suggest_decision` |
| Inspect path metadata, effective decision, inheritance, exceptions and recent history | `get_details` | `read_path` |
| Search/filter/page saved decisions and suggestions, including note text | `list_decisions` / `list_suggestions` | `list_decisions` / `list_suggestions` |
| Read complete saved path/note/reason/history text beyond a preview | `read_review_text` | `read_review_text` |
| Inspect or answer the one-question interview; override, split, defer or restore deferred work | `get_assisted_review`; `mutate`: `assist` / `assistAnswer` | `get_assisted_review` / `control_assisted_review` |
| Start, pause or resume metadata inventory; inspect progress/gaps | `scan` / `get_status` | `inventory_control` / `inventory_status` |
| Count/hash all roots or a subtree; pause/resume/restart; inspect progress | `audit_control` / `audit_status` | `checksum_control` / `checksum_status` |
| Read change notifications and mark specified/all notifications read | `audit_notifications` / `audit_read_notifications` | `read_notifications` / `mark_notifications_read` |
| Activate a notification's source location | mark read, then review `openMain` | `mark_notifications_read`, then `navigate_review` |
| Resize/collapse/reopen sidebars, size/fill folder columns, restore the strip, expand/collapse Path/Search | `mutate`: review `setLayout` / `setView` | `control_review_view` |
| Enable/disable Follow AI's movement | `control_ui`: `setFollowAI` | `control_ui` |
| Inspect eligible copy entries, queue signatures, destination and job progress | `copy_queue_snapshot` | `get_copy_queue` |
| Start copying in order; copy everything currently marked Migrate now | `control_copy_queue`: `start`, mode `ordered` / `all` | `control_copy_queue` |
| Explicitly copy a Last Moment path scope | `control_copy_queue`: `start`, mode `last-moment` | `control_copy_queue` |
| Request copy pause or resume a stopped current-session job | `control_copy_queue`: `pause` / `resume` | `control_copy_queue` |
| Enable/disable this session's debounced Auto-copy | `control_ui`: `setAutoCopy` | `control_ui` |
| Open/close Assistance, decision, context, history, saved-record, coverage, gateway or conflict panels | `control_ui`: `openPanel` / `closePanels` | `control_ui` |
| Change the visible saved-record search/filter/page | `control_ui`: `setSavedRecords` | `control_ui` |
| Show/dismiss notification preview/full list and change its page | `control_ui`: `setNotifications` | `control_ui` |
| Open/close the assistance override chooser, row menu, copy details or gateway details | `control_ui`: `setAssistanceOverride` / `setRowMenu` / `setCopyDetails` / `setGatewayDetails` | `control_ui` |
| Inspect pending saves, buffered notes/view/path drafts and save/conflict state | `get_ui_state`; `control_ui`: `listPending` / `readPendingText` | `get_ui_state` / `control_ui` |
| Flush pending saves, safely retry a failed save, compare a conflict, apply exact retained edits to a reviewed revision | `control_ui`: `flushPending` / `retryPending` / `compareConflict` / `retryReviewed` | `control_ui` |
| Dismiss status or error feedback while retaining user work | `control_ui`: `dismissFeedback` | `control_ui` |
| Gracefully close the window, explicitly exit, or retry/cancel a waiting exit ticket | `control_ui`: `closeWindow` / `exitApp` / `retryExit` / `cancelExit` | `control_ui` |
| Select or clear exact rows; open selection review | `control_ui`: `setEntrySelection` / `clearEntrySelection` / `openSelectionReview` | `control_ui` |
| Review grouped history; atomically save or undo a reviewed group | `read_review_group` / `mark_review_selection` / `undo_review_group` | `get_review_group`; `mutate`: `bulkClassify` / `undoGroup` |
| Review, save and open a manual mirror binding | `preview_mirror_binding`, `save_mirror_binding`, `control_ui`: open Mirror panel | `preview_mirror_binding`, `mutate`: `mirrorBind`, `control_ui` |
| Compare source and mirror, cancel, inspect evidence, prepare and confirm exact membership | `get_mirror_comparison`, `start_mirror_comparison`, `cancel_mirror_comparison`, `read_mirror_evidence`, `prepare_mirror_confirmation`, `confirm_mirror_membership` | Mirror status, comparison and evidence methods; `mutate`: `mirrorConfirm` |
| Review and explicitly copy one file beside an occupied original destination | `preview_copy_conflict`, `control_copy_queue`: `startSeparate` | `preview_copy_conflict`, `control_copy_queue` |
| Choose external or local assistance, revalidate, and submit one proposal | `control_adaptive_interview`, `get_adaptive_question_packet`, `propose_adaptive_question` | `get_adaptive_question_packet`; `mutate`: `adaptiveControl` / `adaptiveProposal` |
| Inspect and reconcile independent copy history | `get_copy_queue`, `read_copy_effect_history`, `reconcile_copy_history` | `copy_queue_snapshot`, `read_copy_effect_history`, `reconcile_copy_history` |
| Inspect a damaged journal and a valid recovery backup | `get_recovery_state` | `get_recovery_state` |
| Explicitly restore that reviewed backup, preserving the displaced original | `recover_review` | `recover_review` |

These mappings expose supported outcomes. For example, querying the next
attention target does not silently navigate the desktop; the caller composes the
returned target with navigation. Opening a decision menu does not apply a mark.
Keyboard focus, the current details target, and the per-root selection are separate. Ctrl+Space toggles an exact member of the selection.

## Confirmation and job semantics

Agent changes require explicit user direction, recorded with agent provenance and
a reason. A caller's `user_instruction` assertion is not independent proof of a
physical human gesture. Suggestions remain separate. UI and API entry points
retain their normal path, volume, instance, revision and operation-ID checks.

Copy start/resume additionally revalidate physical source and destination
identities. Prepared copy requests bind the queue, configured source volumes and
destination. Durable intent and an accepted receipt are saved before the worker
is released to perform copy I/O. An exact retry returns that receipt without
launching another job. A pause request can remain pending while a file finishes;
inspect job status. An interrupted prior-session job has uncertain completion
and needs a newly confirmed start. Auto-copy starts off each session and enabling
it requires current copy fences and confirmation.

Both manual backup states stay outside automated copy batches. `all` includes
Migrate now only. Last Moment requires a separate explicit path scope. Ordinary copy
modes preserve the configured source-relative path to the destination, use the
same no-replace/hash checks and expose no deletion or wipe action.

A separately named conflict copy requires its own reviewed filename and
fingerprint. The ordinary original-path queue remains unresolved for that file.
The independent copy ledger records durable admission before release. A
recovered journal missing newer admissions blocks further copying until the
user reviews and reconciles the exact history. See [recovery](RECOVERY.md).

Recovery is available only to the exclusive journal owner with a valid backup
and a still-matching malformed/missing original. Its request binds both file
hashes and the current native instance. It preserves the original and stores a
durable provenance-bearing receipt in the recovered journal. Normal editing,
inventory, audit and copying remain blocked while the journal is unhealthy.

## Bounded inspection

Folder and saved-record lists are paginated. Saved records accept a page size of
1–100 and expose opaque stable record IDs. Long text has a hash, total byte count
and UTF-8 continuation; subsequent reads bind its revision and text hash. Recent
path history and child-exception projections retain their shared desktop/native
bounds rather than exposing arbitrary raw journal files.

UI reports contain compact pending-request metadata. Fingerprint-fenced
`listPending` and `readPendingText` inspect complete retained request JSON and
unsent drafts without submitting them. Reports are limited to 2 MiB and command
results to 64 KiB. Terminal receipts may be compacted to local session-scoped
storage; exact replay retains the outcome and explicitly reports expired results.
An expired read can be repeated under a fresh inspection ID and current fences.
Archiving does not permit a previous effect to run again.

## Authority and input-mechanics boundaries

Pointer motion, hover/press decoration, transient resize previews, text selection,
keyboard focus traversal and highlighted menu items are input mechanics. Their
committed application outcomes are exposed above; the API does not synthesize
arbitrary mouse/keyboard events or execute scripts in the window. Native/browser
text editing, OS clipboard behavior, title-bar move/minimize/maximize and OS
security/authentication prompts remain with the OS. Closing the app still uses
its normal pending-save guard. The supported flush/conflict operations let a
caller protect retained work before a normal close.

Trusted UI registration/reporting IPC is internal. HTTP/MCP cannot forge a
rendered-state report, install a replacement UI session, or bypass confirmations.
Commands requiring a rendered window report an unavailable/stale session when
there is no live reporting renderer. Native copy, inventory, audit and recovery
operations have their own backend job state.

The existing UI has no arbitrary destination picker. Its guarded destination
configuration is shared with the APIs; an unrestricted destination setter is not
introduced as a parity shortcut. Historical decisions for another physical
volume are not rebound by drive letter.

## Validation and limits

The 8 November 2026 parity gate passed **94 native tests** and **30 frontend/bridge
tests**, followed by a TypeScript/Vite production build. Native tests cover
authenticated HTTP dispatch, damaged-journal recovery, copy fencing and exact
retry across restart, pause/resume, source/destination substitution, overlapping
starts, save acknowledgement failure, no-replace publication, UI-session
replacement, bounded Unicode inspection and retained receipt behavior. Native
Windows WebView and fresh stdio MCP integration use separate synthetic source,
destination, journal and runtime directories. Their release receipt records the
specific executions; test source alone is not runtime proof. The embedded
production executable passed native UI checks at **940×640** and **1420×920**
with the development server stopped, followed by **nine fresh stdio MCP
copy/recovery checks**. Those exercised actual synthetic copying, deep path
mapping, manual/Last Moment exclusions, stale fences, exact retry across app
restart and damaged-journal recovery preserving newer work. Every fixture closed
normally. UI-only fixtures started zero copy jobs; copy fixtures used synthetic
sources and destinations exclusively.

The real migration journal is not a validation fixture. Source hashes, manual
assertions and copy receipts describe their stated scope; they do not establish
full backup coverage or a successful restore. Unsupported links/streams and
unavailable paths remain gaps. External-editor conflict detection uses
fingerprints, cooperative ownership and guarded replacement retries, not atomic
compare-and-swap against every uncooperative writer.

A configured bridge, fresh 29-tool discovery, a native operation result and a
previously connected client's catalog are separate checks. Existing clients can
retain older tool lists. This app does not restart clients or globally reload
other MCP connections to activate a new catalog.
