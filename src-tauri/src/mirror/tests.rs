use super::*;
use std::{
    fs::{self, OpenOptions},
    os::windows::fs::OpenOptionsExt,
    path::PathBuf,
    sync::atomic::Ordering,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    mirror: PathBuf,
    evidence: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("migration-mirror-{}", manifest::nonce().unwrap()));
        let fixture = Self {
            source: root.join("source"),
            mirror: root.join("mirror"),
            evidence: root.join("evidence"),
            root,
        };
        for directory in [&fixture.source, &fixture.mirror, &fixture.evidence] {
            fs::create_dir_all(directory).unwrap();
        }
        fixture
    }

    fn env(&self) -> Environment<'_> {
        Environment::Fixture {
            source_root: &self.source,
            mirror_root: &self.mirror,
        }
    }

    fn bind(&self) -> Binding {
        bind(
            self.source.to_str().unwrap(),
            self.mirror.to_str().unwrap(),
            self.env(),
        )
        .unwrap()
    }

    fn compare(&self, binding: &Binding) -> Receipt {
        verify(
            binding,
            self.env(),
            &self.evidence,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap()
    }

    fn file(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn equal(&self, relative: &str, bytes: &[u8]) {
        Self::file(&self.source, relative, bytes);
        Self::file(&self.mirror, relative, bytes);
    }

    fn manifest(&self, receipt: &Receipt) -> PathBuf {
        self.evidence
            .join(format!("mirror-{}.ndjson", receipt.manifest_digest))
    }

    fn rows(&self, receipt: &Receipt) -> Vec<Record> {
        fs::read_to_string(self.manifest(receipt))
            .unwrap()
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[test]
fn mirror_maps_the_selected_roots_and_checks_full_bytes_and_each_difference_class() {
    let f = Fixture::new();
    let source = f.source.join("Projects");
    let mirror = f.mirror.join("Saved").join("My Projects");
    Fixture::file(&source, r"deep\nested\same.txt", b"abcd");
    Fixture::file(&mirror, r"deep\nested\same.txt", b"abcd");
    Fixture::file(&source, "same-size.txt", b"abcd");
    Fixture::file(&mirror, "same-size.txt", b"abce");
    Fixture::file(&source, "missing.txt", b"keep");
    Fixture::file(&mirror, "extra.txt", b"past");
    let binding = bind(source.to_str().unwrap(), mirror.to_str().unwrap(), f.env()).unwrap();
    assert_ne!(binding.source.volume_id, binding.mirror.volume_id);
    assert_eq!(binding.source.root, source.to_string_lossy());
    assert_eq!(binding.mirror.root, mirror.to_string_lossy());
    let mut observed = Vec::new();
    let receipt = verify(
        &binding,
        f.env(),
        &f.evidence,
        &AtomicBool::new(false),
        |progress| {
            observed.push(progress.clone());
        },
    )
    .unwrap();
    assert_eq!(
        (
            receipt.counts.equal,
            receipt.counts.missing,
            receipt.counts.different,
            receipt.counts.extra
        ),
        (1, 1, 1, 1)
    );
    assert!(receipt.coverage_complete);
    assert!(!receipt.source_matches);
    assert_eq!((receipt.checked_files, receipt.checked_bytes), (6, 24));
    assert_eq!(observed.first().unwrap().phase, "comparing");
    assert!(!observed.first().unwrap().total_known);
    assert!(observed.last().unwrap().total_known);
    assert_eq!(observed.last().unwrap().queued_files, 0);
    let bytes = fs::read(f.manifest(&receipt)).unwrap();
    assert_eq!(store::hash(&bytes), receipt.manifest_digest);
    check_receipt(&f.evidence, &binding, &receipt).unwrap();
    let rows = f.rows(&receipt);
    let changed = rows
        .iter()
        .find(|row| row.relative == "same-size.txt")
        .unwrap();
    assert_eq!(changed.verdict, Verdict::Different);
    assert_ne!(changed.source, changed.mirror);
    assert_eq!(fs::read(source.join("same-size.txt")).unwrap(), b"abcd");
    assert_eq!(fs::read(mirror.join("same-size.txt")).unwrap(), b"abce");
    let page = serde_json::to_value(
        read_evidence_page(&f.evidence, &binding, &receipt, 0, 2, true).unwrap(),
    )
    .unwrap();
    assert_eq!(page["matched"], 3);
    assert_eq!(page["nextOffset"], 2);
    let next = serde_json::to_value(
        read_evidence_page(&f.evidence, &binding, &receipt, 2, 2, true).unwrap(),
    )
    .unwrap();
    assert_eq!(next["entries"].as_array().unwrap().len(), 1);
    assert!(next["nextOffset"].is_null());
    assert!(read_evidence_page(&f.evidence, &binding, &receipt, 0, 101, true).is_err());
}

#[test]
fn hash_workers_handle_multiple_bounded_batches_and_exact_path_spelling() {
    let f = Fixture::new();
    for i in 0..68 {
        f.equal(&format!("sub\\file-{i:03}.txt"), b"hash in bounded batches");
    }
    let data = vec![0x3d; 2 * 1024 * 1024 + 3];
    f.equal("large.bin", &data);
    Fixture::file(&f.source, "Case.TXT", b"same spelling matters");
    Fixture::file(&f.mirror, "case.txt", b"same spelling matters");
    let binding = f.bind();
    let receipt = f.compare(&binding);
    assert_eq!(receipt.counts.equal, 69);
    assert_eq!(receipt.counts.unsupported, 1);
    assert_eq!(receipt.counts.coverage_gaps, 1);
    assert_eq!(receipt.checked_files, 138);
    let rows = f.rows(&receipt);
    let large = rows.iter().find(|row| row.relative == "large.bin").unwrap();
    assert_eq!(large.source.as_ref().unwrap().sha256, store::hash(&data));
    assert_eq!(large.source, large.mirror);
    let gap = rows.iter().find(|row| row.relative == "Case.TXT").unwrap();
    assert_eq!(gap.mirror_relative.as_deref(), Some("case.txt"));
    assert_eq!(gap.verdict, Verdict::Unsupported);
}

#[test]
fn junctions_are_not_followed_by_binding_or_comparison() {
    use std::os::windows::process::CommandExt;
    let f = Fixture::new();
    let elsewhere = f.root.join("outside");
    Fixture::file(&elsewhere, "must-not-read.txt", b"outside fixture scope");
    let link = f.source.join("junction");
    let output = std::process::Command::new("cmd.exe")
        .args([
            "/c",
            "mklink",
            "/J",
            link.to_str().unwrap(),
            elsewhere.to_str().unwrap(),
        ])
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(bind(link.to_str().unwrap(), f.mirror.to_str().unwrap(), f.env()).is_err());
    let binding = f.bind();
    let receipt = f.compare(&binding);
    assert_eq!(receipt.counts.unsupported, 1);
    assert_eq!(receipt.checked_files, 0);
    assert!(!f
        .rows(&receipt)
        .iter()
        .any(|row| row.relative.contains("must-not-read")));
}

#[test]
fn inaccessible_peers_and_named_streams_are_gaps_instead_of_absence() {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    };
    let f = Fixture::new();
    f.equal("locked.txt", b"contents");
    let locked = OpenOptions::new()
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(f.mirror.join("locked.txt"))
        .unwrap();
    f.equal(r"Blocked\hidden.txt", b"present on both sides");
    let blocked = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
        .share_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(f.mirror.join("Blocked"))
        .unwrap();
    f.equal("stream.txt", b"unnamed");
    fs::write(
        format!(
            "{}:fixture-side-stream",
            f.source.join("stream.txt").display()
        ),
        b"also data",
    )
    .unwrap();
    let binding = f.bind();
    let receipt = f.compare(&binding);
    drop(blocked);
    drop(locked);
    assert_eq!(receipt.counts.missing, 0);
    assert_eq!(receipt.counts.extra, 0);
    assert_eq!(receipt.counts.unreadable, 2);
    assert_eq!(receipt.counts.unsupported, 1);
    assert_eq!(receipt.counts.coverage_gaps, 3);
    assert!(!receipt.source_matches);
    let rows = f.rows(&receipt);
    assert!(!rows.iter().any(|row| row.relative == r"Blocked\hidden.txt"));
    assert!(rows.iter().any(|row| row.relative == "Blocked"
        && row.directory
        && row.verdict == Verdict::Unreadable));
    assert!(rows.iter().any(|row| row.relative == "stream.txt"
        && row
            .source_gap
            .as_ref()
            .is_some_and(|gap| gap.verdict == Verdict::Unsupported)));
}

#[test]
fn cancellation_and_replacement_do_not_publish_a_successful_comparison() {
    let f = Fixture::new();
    f.equal("replace.txt", b"same");
    let binding = f.bind();
    let cancel = AtomicBool::new(false);
    let cancelled = verify(&binding, f.env(), &f.evidence, &cancel, |progress| {
        if progress.queued_files > 0 {
            cancel.store(true, Ordering::Release);
        }
    });
    assert!(cancelled.unwrap_err().contains("cancelled"));
    assert_eq!(fs::read_dir(&f.evidence).unwrap().count(), 0);

    let mut replaced = false;
    let receipt = verify(
        &binding,
        f.env(),
        &f.evidence,
        &AtomicBool::new(false),
        |progress| {
            if !replaced && progress.queued_files > 0 {
                fs::rename(f.source.join("replace.txt"), f.source.join("original.txt")).unwrap();
                Fixture::file(&f.source, "replace.txt", b"same");
                replaced = true;
            }
        },
    )
    .unwrap();
    assert!(replaced);
    assert!(!receipt.coverage_complete);
    let rows = f.rows(&receipt);
    assert!(rows
        .iter()
        .any(|row| row.relative == "replace.txt" && row.verdict == Verdict::ChangedDuringRead));
    assert!(rows
        .iter()
        .any(|row| row.relative.is_empty() && row.verdict == Verdict::ChangedDuringRead));
}

#[test]
fn binding_rejects_unsafe_scopes_stale_identities_and_nested_evidence_output() {
    let f = Fixture::new();
    f.equal("kept.txt", b"kept");
    let binding = f.bind();
    assert!(bind(
        f.source.to_str().unwrap(),
        f.source.to_str().unwrap(),
        f.env()
    )
    .is_err());
    assert!(bind(r"E:\\", f.mirror.to_str().unwrap(), f.env()).is_err());
    assert!(bind(
        f.evidence.to_str().unwrap(),
        f.mirror.to_str().unwrap(),
        f.env()
    )
    .is_err());
    let physical = filesystem_volume(&f.source);
    assert!(bind(
        f.source.to_str().unwrap(),
        f.mirror.to_str().unwrap(),
        Environment::Live {
            review_volumes: &[physical]
        }
    )
    .unwrap_err()
    .contains("different Windows volume"));
    let nested = f.source.join("evidence");
    fs::create_dir(&nested).unwrap();
    assert!(
        create_mirror_evidence_directory(&f.source, "unsafe-child", &binding, f.env()).is_err()
    );
    assert!(!f.source.join("unsafe-child").exists());
    let generated =
        create_mirror_evidence_directory(&f.root, "safe-outcome", &binding, f.env()).unwrap();
    assert_eq!(generated, f.root.join("safe-outcome"));
    assert_eq!(
        create_mirror_evidence_directory(&f.root, "safe-outcome", &binding, f.env()).unwrap(),
        generated
    );
    assert!(
        verify(&binding, f.env(), &nested, &AtomicBool::new(false), |_| {})
            .unwrap_err()
            .contains("outside both")
    );
    assert_eq!(fs::read_dir(&nested).unwrap().count(), 0);
    fs::rename(&f.mirror, f.root.join("previous-mirror")).unwrap();
    fs::create_dir(&f.mirror).unwrap();
    assert!(revalidate_binding(&binding, f.env())
        .unwrap_err()
        .contains("identity changed"));
    assert_eq!(fs::read_dir(&f.evidence).unwrap().count(), 0);
}

fn filesystem_volume(path: &Path) -> Volume {
    crate::filesystem::volume(&path.to_str().unwrap()[..3])
}

#[test]
fn immutable_outcomes_accept_exact_retries_and_refuse_conflicts_or_link_aliases() {
    let f = Fixture::new();
    let name = "run-fixture.json";
    let content = b"{\"phase\":\"complete\"}";
    publish_evidence_file(&f.evidence, name, content).unwrap();
    publish_evidence_file(&f.evidence, name, content).unwrap();
    assert!(publish_evidence_file(&f.evidence, name, b"conflicting bytes").is_err());
    assert_eq!(read_evidence_file(&f.evidence, name, 100).unwrap(), content);
    assert!(read_evidence_file(&f.evidence, name, 3).is_err());
    assert!(publish_evidence_file(&f.evidence, r"..\outside.json", b"must not write").is_err());
    assert_eq!(fs::read_dir(&f.evidence).unwrap().count(), 1);

    let outside = Fixture::file(&f.source, "private-fixture.json", b"outside evidence");
    let alias = "run-alias.json";
    fs::hard_link(&outside, f.evidence.join(alias)).unwrap();
    assert!(read_evidence_file(&f.evidence, alias, 100).is_err());
    assert!(publish_evidence_file(&f.evidence, alias, b"must not overwrite").is_err());
    assert_eq!(fs::read(outside).unwrap(), b"outside evidence");
}

#[test]
fn name_witness_rejects_same_count_replacement_even_when_directory_stamp_is_accepted() {
    let f = Fixture::new();
    f.equal("old.txt", b"same directory count");
    let binding = f.bind();
    let receipt = f.compare(&binding);
    let directory = f
        .rows(&receipt)
        .into_iter()
        .find(|row| row.relative.is_empty())
        .unwrap();
    let witness = directory.source_membership.unwrap();
    fs::rename(f.source.join("old.txt"), f.source.join("new.txt")).unwrap();
    let (_, current, _) =
        windows::open_binding(&binding.source.root, &binding.mirror.root, f.env()).unwrap();
    let metadata_stamp = current.root_stamp();
    let cancel = AtomicBool::new(false);
    // Give the checker a metadata stamp it will accept, as on a filesystem
    // that coarsens this rename into the previous timestamp.
    assert!(current
        .enumerate_names("", &metadata_stamp, &cancel, |_| Ok(()))
        .unwrap()
        .is_ok());
    let guarded = windows::guard_runtime(&f.evidence).unwrap();
    let mut checker = membership::Checker::new(&guarded).unwrap();
    let gap = checker
        .recheck(&current, "", &metadata_stamp, &witness, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(gap.verdict, Verdict::ChangedDuringRead);
    assert!(gap
        .message
        .contains("Exact directory membership names changed"));
}

#[test]
fn legacy_manifest_is_inspectable_but_requires_recomparison_for_grouped_confirmation() {
    use serde_json::Value;

    let f = Fixture::new();
    f.equal("kept.txt", b"dated historical evidence");
    let binding = f.bind();
    let receipt = f.compare(&binding);
    assert_eq!(receipt.schema, 2);
    let contents = fs::read_to_string(f.manifest(&receipt)).unwrap();
    let mut previous = String::new();
    for (index, line) in contents.lines().enumerate() {
        let mut row: Value = serde_json::from_str(line).unwrap();
        if index == 0 {
            row["schema"] = 1.into();
        } else {
            let record = row.as_object_mut().unwrap();
            record.remove("sourceMembership");
            record.remove("mirrorMembership");
        }
        previous.push_str(&serde_json::to_string(&row).unwrap());
        previous.push('\n');
    }
    let legacy = Receipt {
        schema: 1,
        manifest_digest: store::hash(previous.as_bytes()),
        ..receipt
    };
    fs::write(f.manifest(&legacy), previous).unwrap();
    check_receipt(&f.evidence, &binding, &legacy).unwrap();
    assert!(prepare_membership(&f.evidence, &binding, &legacy, &[])
        .unwrap_err()
        .contains("fresh comparison"));
}

#[test]
fn frozen_membership_is_exact_and_stale_or_corrupt_evidence_cannot_confirm() {
    let f = Fixture::new();
    f.equal(r"Group\kept.txt", b"covered");
    Fixture::file(&f.source, r"Group\deferred.txt", b"intentionally uncovered");
    let binding = f.bind();
    let receipt = f.compare(&binding);
    assert!(prepare_membership(&f.evidence, &binding, &receipt, &[])
        .unwrap_err()
        .contains("unresolved"));
    assert!(
        prepare_membership(&f.evidence, &binding, &receipt, &["Never existed".into()]).is_err()
    );
    let exceptions = [r"Group\deferred.txt".into()];
    let prepared = prepare_membership(&f.evidence, &binding, &receipt, &exceptions).unwrap();
    assert_eq!(prepared.covered_files, 1);
    let mut window = EvidenceWindow {
        instance_id: "current-fixture-instance".into(),
        binding_id: binding.binding_id.clone(),
        manifest_digest: receipt.manifest_digest.clone(),
        source_sequence: 3,
        mirror_sequence: 2,
        source_continuous: true,
        mirror_continuous: true,
    };
    let mut fence = ConfirmationFence {
        confirmed: true,
        expected_revision: 7,
        current_revision: 7,
        expected_membership_digest: &prepared.membership_digest,
        verified: &window,
        current: &window,
    };
    assert_eq!(
        confirm_membership(&f.evidence, &binding, &receipt, &exceptions, &fence).unwrap(),
        prepared
    );
    revalidate_membership(
        &f.evidence,
        &binding,
        &receipt,
        f.env(),
        &prepared,
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut reopened = window.clone();
    reopened.instance_id = "reopened-fixture-instance".into();
    fence.current = &reopened;
    assert!(
        confirm_membership(&f.evidence, &binding, &receipt, &exceptions, &fence)
            .unwrap_err()
            .contains("historical")
    );
    let mut gap = window.clone();
    gap.mirror_continuous = false;
    fence.current = &gap;
    assert!(confirm_membership(&f.evidence, &binding, &receipt, &exceptions, &fence).is_err());
    let mut changed = window.clone();
    changed.mirror_sequence += 1;
    fence.current = &changed;
    assert!(confirm_membership(&f.evidence, &binding, &receipt, &exceptions, &fence).is_err());
    window.manifest_digest = "different-scan".into();
    assert!(!window.matches_current(&binding, &receipt, &window));

    Fixture::file(&f.source, r"Group\new.txt", b"added after comparison");
    assert!(revalidate_membership(
        &f.evidence,
        &binding,
        &receipt,
        f.env(),
        &prepared,
        &AtomicBool::new(false)
    )
    .is_err());
    check_receipt(&f.evidence, &binding, &receipt).unwrap();
    let path = f.manifest(&receipt);
    let mut data = fs::read(&path).unwrap();
    data.pop();
    fs::write(path, data).unwrap();
    assert!(check_receipt(&f.evidence, &binding, &receipt).is_err());
}
