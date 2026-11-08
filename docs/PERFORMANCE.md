# Inventory progress and navigation reuse

## Why assistance waited

AI Assistance waits for a completed metadata inventory before selecting review
questions. This is local filesystem work, not a hosted model request. The
interview already keeps its derived directory tree in memory for the same scope
and completed scan. That cache does not replace the folder browser's listings.

Previously, the metadata inventory used one directory worker while the separate
count-and-checksum audit already used a bounded worker pool. The two jobs cannot
be exchanged directly: the audit reads file contents and computes SHA-256, while
the interview needs the metadata index and its conservative coverage rules.

The metadata scanner now follows the same bounded, durable-work architecture,
with up to four directory workers. It reuses guarded refresh and the existing
SQLite queue, atomically claims directories, reuses each folder's database
connection, and commits final entries with their summary. Pausing drains the
workers; unfinished claims are recoverable after a restart. It does not weaken
reparse-point, source-change, inaccessible-directory or backup-verification rules.

## Truthful progress

Assistance reads the same transactional inventory status as the scan controls.
The panel shows processed/queued/active directories, worker count, files found,
logical bytes, current path, recorded gaps, and time since the scan started
(including pauses). Discovery has no reliable total until it finishes, so its
progress bar is indeterminate and does not invent a percentage or ETA.

## Folder cache

The root browser and preview columns share a session-only LRU cache of at most
64 pages and approximately 8 MiB of serialized payload, with a 15-second TTL.
Concurrent identical requests share one native read. Query keys include path,
search, filter, and page. View-only journal saves do not discard cached pages.

Relevant journal changes, watcher/audit evidence, inventory progress, source
scope changes, and explicit refresh invalidate reuse. An explicit refresh also
bypasses the cache. Immediate-directory metadata observations are tracked
separately (at most 256 directories, 15-second TTL). Source watcher events or
metadata expiry trigger fresh directory enumeration, while ordinary hash progress
and decision edits only recalculate the derived listing. Original observation timestamps and stale/partial labels
remain unchanged. Detail reads, action planning and mutations stay authoritative
and uncached. This is a responsive browsing cache, not proof of current backup.

## Durable checkpoints and hash reuse

The directory inventory and content-hash audit use separate SQLite stores. The
inventory saves completed directory summaries and metadata fingerprints; these
are not SHA-256 hashes of file contents. Completed queue entries survive a resume,
while unfinished claimed directories are returned to the queue after interruption.

The audit saves completed SHA-256 digests with source identity, metadata and
verification time. Resume preserves those completed records and hashes unfinished
files. A file interrupted mid-hash restarts from byte zero. A fresh comparison
intentionally reads contents again, preserving the previous digest for comparison;
size and timestamp alone cannot prove unchanged contents.

Completed-hash result batches now use SQLite FULL synchronous commits. Digest,
verification time and aggregate counters commit atomically. Transient discovery,
claims and progress keep NORMAL mode to avoid paying for a disk flush on every
queue update. FULL flushes the checkpoint transaction and earlier WAL records,
subject to the storage device honoring flushes; it is not an absolute hardware
guarantee. The hash-progress details show saved, queued and in-flight file counts.

Recorded hashes remain historical evidence after an unobserved watcher interval.
Restart and resume preserve work but do not certify current freshness or an
external backup. The existing reconciliation requirement is retained.

## Measurements and validation

Synthetic Windows metadata fixture: 161 directories and 6,400 files, exact
counts and no gaps.

- Original serial implementation: 4,995 ms (one baseline run).
- Optimized one-worker runs: 5,476 / 3,442 / 3,924 ms; median 3,924 ms.
- Optimized four-worker runs: 3,036 / 3,428 / 3,348 ms; median 3,348 ms.

Four-worker median was about 15% below the optimized single-worker median. The
comparison with the single original baseline was about 33%; this is a small
synthetic fixture, not a promise about a full drive, cold caches or other disks.

The native suite passed 96 tests with the benchmark separately executed. Tests
cover atomic claims, pause/resume with overlapping workers, restart recovery and
exact counts. Frontend regression tests cover cache hits, in-flight sharing,
invalidation, stale completions, TTL, eviction, oversized values, retry after
failure, query isolation, and progress parsing without a false percentage.

Checkpoint regressions additionally verify unchanged saved digests/timestamps on
restart, rehashing interrupted files, rollback without partially saved counts, and
watcher-only invalidation sequences. A synthetic commit microbenchmark measured
100 four-hash transactions at median 425 ms with FULL versus 3.82 ms with NORMAL.
That is commit overhead, not whole-scan throughput, and motivated applying FULL
only to completed-hash batches.

Native WebView fixtures exercised progress at 940×640 and 1420×920, including
reduced motion, forced colors, pause/resume and closing/reopening the interview.
The 241-directory/9,600-file fixture resumed with exact counts. A first run saw a
late Windows source-change event after completion; the app correctly kept that
scope in needs-scan rather than claiming a fresh question. Later fixtures wait for
the initial event stream to settle. These checks do not alter the live journal.

These source changes do not replace or restart an already-running application.
An active scan continues using the executable that started it until a separately
coordinated app update.
