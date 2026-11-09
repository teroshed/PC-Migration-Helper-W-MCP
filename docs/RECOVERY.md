# Journal recovery and rollback

Keep the original journal, its recovery copy and the current independent copy history. A previous journal can contain a valid set of marks while omitting copy requests that already changed a destination.

## Save a matched set

1. Close the app normally. Wait for the window to finish saving and exit.
2. Preserve the executable and its launch arguments, the journal and its `.json.bak` recovery copy, any journal format marker and pre-upgrade archives, and the app data directory. If you use a separate `--journal-file`, keep its adjacent sidecars too.
3. Keep the current `.json.copy-authority` marker and `.json.copy-effects.sqlite` ledger together. Do not replace them with an older pair during a rollback. Keep per-file copy receipts and mirror/interview evidence if you need their historical details.

The first operation that needs journal version 2 prepares a compatible recovery copy and preserves the exact preceding version 1 bytes. Earlier releases do not understand a version 2 journal or recovery copy. Use a build that understands the saved format.

## Recover a damaged journal

Use the app’s recovery view to inspect the backup, its format and file fingerprints. Confirm recovery only when the displayed original and backup are the intended pair. The app preserves the displaced original. Recovery restores journal records; it does not recreate files on the source or destination.

## Reconcile copy effects after rollback

Use the current enforcing build for rollback inspection. You can launch it with `--copy-inspection-only` to keep copy starts disabled throughout that process. An archived executable that predates the independent ledger is unsuitable for copy-capable rollback.

If the restored journal omits newer copy admissions, the queue blocks new starts, resume and Auto-copy. Review the independent receipts, the current marks and the original destination before acknowledging the exact history. Reconciliation preserves the old operation IDs and does not run a copy or verify that the destination is complete. An exact retry of an old ID cannot start a new pass.

If the ledger is missing or damaged, copying remains blocked. Preserve the originals and restore the matched independent ledger before any further copy. A fresh copy plan requires a healthy ledger, current drives and explicit authorization.

Source hashes, mirror comparisons and copy receipts cover the stated files and streams. Verify a restore of the data and application state you need before retiring the source drive.
