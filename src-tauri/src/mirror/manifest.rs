use super::{
    membership::{Checker, Witness},
    scan,
    windows::{self, EvidenceDirectory},
    Binding, Counts, Environment, Progress, Receipt, Record, Verdict,
};
use crate::store;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Read, Write},
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS},
    Storage::FileSystem::{MoveFileExW, FILE_SHARE_READ, MOVEFILE_WRITE_THROUGH},
};

const FORMAT: &str = "pc-migration-mirror-manifest";
const MANIFEST_SCHEMA: u32 = 2;
const LINE_MAX: u64 = 1024 * 1024;
const EXCEPTION_MAX: usize = 1000;
const PAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Header {
    format: String,
    schema: u32,
    binding: Binding,
    started_at: String,
    finished_at: String,
    counts: Counts,
    checked_files: u64,
    checked_bytes: u64,
}

impl Header {
    fn receipt(&self, digest: String) -> Receipt {
        Receipt {
            schema: self.schema,
            manifest_digest: digest,
            binding_id: self.binding.binding_id.clone(),
            started_at: self.started_at.clone(),
            finished_at: self.finished_at.clone(),
            counts: self.counts.clone(),
            checked_files: self.checked_files,
            checked_bytes: self.checked_bytes,
            coverage_complete: self.counts.coverage_complete(),
            source_matches: self.counts.source_matches(),
        }
    }
}

pub(super) fn nonce() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(store::error)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct StagedFile(PathBuf);
impl StagedFile {
    fn create(directory: &EvidenceDirectory) -> Result<(Self, File), String> {
        let temporary = directory
            .path
            .join(format!(".mirror-evidence-{}.tmp", nonce()?));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ)
            .open(&temporary)
            .map_err(store::error)?;
        Ok((Self(temporary), file))
    }

    fn move_to(&self, destination: &Path) -> Result<(), String> {
        // No REPLACE_EXISTING and no COPY_ALLOWED. Both paths have the same
        // guarded parent and the destination is immutable.
        let renamed = unsafe {
            MoveFileExW(
                windows::wide_path(&self.0).as_ptr(),
                windows::wide_path(destination).as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        };
        if renamed == 0 {
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(code) if code == ERROR_ALREADY_EXISTS as i32 || code == ERROR_FILE_EXISTS as i32)
            {
                return Err(format!("Could not publish evidence: {error}"));
            }
        }
        Ok(())
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn named_path(directory: &EvidenceDirectory, name: &str) -> Result<PathBuf, String> {
    if name.len() > 255 || name.contains('\\') || !windows::relative_supported(name) {
        return Err("An evidence sidecar name must be one supported file name.".into());
    }
    Ok(directory.path.join(name))
}

fn read_small(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file = windows::open_evidence(path)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(store::error)?;
    if bytes.len() > limit {
        return Err("The saved evidence sidecar exceeds the supported size.".into());
    }
    Ok(bytes)
}

pub(crate) fn read_evidence_file(
    directory: &Path,
    name: &str,
    limit: usize,
) -> Result<Vec<u8>, String> {
    if !(1..=LINE_MAX as usize).contains(&limit) {
        return Err("Choose a supported maximum evidence size.".into());
    }
    let directory = windows::guard_runtime(directory)?;
    read_small(&named_path(&directory, name)?, limit)
}

pub(crate) fn publish_evidence_file(
    directory: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    if bytes.len() as u64 > LINE_MAX {
        return Err("The evidence sidecar exceeds the supported size.".into());
    }
    let directory = windows::guard_runtime(directory)?;
    let destination = named_path(&directory, name)?;
    let (staged, mut file) = StagedFile::create(&directory)?;
    file.write_all(bytes).map_err(store::error)?;
    file.sync_all().map_err(store::error)?;
    drop(file);
    staged.move_to(&destination)?;
    if read_small(&destination, LINE_MAX as usize)? != bytes {
        return Err("Different evidence already exists for this immutable name.".into());
    }
    Ok(())
}

fn path(directory: &EvidenceDirectory, prefix: &str, digest: &str) -> Result<PathBuf, String> {
    if !matches!(prefix, "mirror" | "source") {
        return Err("Unsupported evidence manifest format.".into());
    }
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("The evidence digest is not a lowercase SHA-256 value.".into());
    }
    Ok(directory.path.join(format!("{prefix}-{digest}.ndjson")))
}

fn write_line(file: &mut BufWriter<File>, digest: &mut Sha256, text: &[u8]) -> Result<(), String> {
    if text.len() as u64 >= LINE_MAX || text.contains(&b'\n') {
        return Err("An evidence record exceeds the supported manifest row format.".into());
    }
    file.write_all(text).map_err(store::error)?;
    file.write_all(b"\n").map_err(store::error)?;
    digest.update(text);
    digest.update(b"\n");
    Ok(())
}

pub(super) fn publish(
    directory: &EvidenceDirectory,
    binding: &Binding,
    progress: &Progress,
    started_at: String,
    cancel: &AtomicBool,
    row: impl FnMut() -> Result<Option<String>, String>,
) -> Result<Receipt, String> {
    scan::check_cancel(cancel)?;
    let header = Header {
        format: FORMAT.into(),
        schema: MANIFEST_SCHEMA,
        binding: binding.clone(),
        started_at,
        finished_at: store::now(),
        counts: progress.counts.clone(),
        checked_files: progress.checked_files,
        checked_bytes: progress.checked_bytes,
    };
    let header_line = serde_json::to_vec(&header).map_err(store::error)?;
    let digest = publish_lines(directory, "mirror", &header_line, cancel, row)?;
    let receipt = header.receipt(digest);
    visit(directory, binding, &receipt, |_| Ok(()))?;
    Ok(receipt)
}

pub(super) fn publish_lines(
    directory: &EvidenceDirectory,
    prefix: &str,
    header_line: &[u8],
    cancel: &AtomicBool,
    mut row: impl FnMut() -> Result<Option<String>, String>,
) -> Result<String, String> {
    scan::check_cancel(cancel)?;
    let (staged, file) = StagedFile::create(directory)?;
    let mut writer = BufWriter::new(file);
    let mut digest = Sha256::new();
    write_line(&mut writer, &mut digest, header_line)?;
    while let Some(row) = row()? {
        scan::check_cancel(cancel)?;
        write_line(&mut writer, &mut digest, row.as_bytes())?;
    }
    writer.flush().map_err(store::error)?;
    writer.get_ref().sync_all().map_err(store::error)?;
    drop(writer);
    let digest = format!("{:x}", digest.finalize());
    let destination = path(directory, prefix, &digest)?;
    scan::check_cancel(cancel)?;
    staged.move_to(&destination)?;
    Ok(digest)
}

fn read_line(reader: &mut BufReader<File>, line: &mut Vec<u8>) -> Result<bool, String> {
    line.clear();
    let count = reader
        .take(LINE_MAX + 1)
        .read_until(b'\n', line)
        .map_err(store::error)?;
    if count == 0 {
        return Ok(false);
    }
    if count as u64 > LINE_MAX || line.last() != Some(&b'\n') {
        return Err("The evidence manifest contains an incomplete or oversized record.".into());
    }
    Ok(true)
}

fn visit(
    directory: &EvidenceDirectory,
    binding: &Binding,
    receipt: &Receipt,
    mut record: impl FnMut(&Record) -> Result<(), String>,
) -> Result<(), String> {
    let mut counts = Counts::default();
    let mut checked_files = 0_u64;
    let mut checked_bytes = 0_u64;
    visit_lines(
        directory,
        "mirror",
        &receipt.manifest_digest,
        |line| {
            let header: Header = serde_json::from_slice(line).map_err(store::error)?;
            if header.format != FORMAT
                || !matches!(header.schema, 1 | MANIFEST_SCHEMA)
                || header.binding != *binding
                || header.receipt(receipt.manifest_digest.clone()) != *receipt
            {
                return Err(
                    "The mirror manifest does not match the saved binding and receipt.".into(),
                );
            }
            Ok(())
        },
        |line| {
            let entry: Record = serde_json::from_slice(line).map_err(store::error)?;
            counts.record(entry.verdict);
            counts.coverage_gaps += u64::from(entry.coverage_gap);
            for digest in [&entry.source, &entry.mirror].into_iter().flatten() {
                checked_files += 1;
                checked_bytes = checked_bytes
                    .checked_add(digest.bytes)
                    .ok_or("Manifest byte count overflow.")?;
            }
            record(&entry)
        },
    )?;
    if counts != receipt.counts
        || checked_files != receipt.checked_files
        || checked_bytes != receipt.checked_bytes
    {
        return Err("Mirror evidence counters do not match the saved receipt.".into());
    }
    Ok(())
}

pub(super) fn visit_lines(
    directory: &EvidenceDirectory,
    prefix: &str,
    expected_digest: &str,
    header: impl FnOnce(&[u8]) -> Result<(), String>,
    mut row: impl FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let path = path(directory, prefix, expected_digest)?;
    let file = windows::open_evidence(&path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut digest = Sha256::new();
    if !read_line(&mut reader, &mut line)? {
        return Err("The evidence manifest is empty.".into());
    }
    header(&line)?;
    digest.update(&line);
    while read_line(&mut reader, &mut line)? {
        digest.update(&line);
        row(&line)?;
    }
    if format!("{:x}", digest.finalize()) != expected_digest {
        return Err("The evidence manifest bytes do not match the saved receipt.".into());
    }
    Ok(())
}

/// Historical inspection validates the saved bytes without making the
/// external volume, source or monitoring available again.
pub(crate) fn check_receipt(
    directory: &Path,
    binding: &Binding,
    receipt: &Receipt,
) -> Result<(), String> {
    let directory = windows::guard_runtime(directory)?;
    visit(&directory, binding, receipt, |_| Ok(()))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceEntry {
    relative: String,
    source_relative: Option<String>,
    mirror_relative: Option<String>,
    verdict: Verdict,
    directory: bool,
    coverage_gap: bool,
    source: Option<super::Digest>,
    mirror: Option<super::Digest>,
    source_membership: Option<Witness>,
    mirror_membership: Option<Witness>,
    problem: Option<String>,
    source_problem: Option<String>,
    mirror_problem: Option<String>,
    source_verdict: Option<Verdict>,
    mirror_verdict: Option<Verdict>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EvidencePage {
    entries: Vec<EvidenceEntry>,
    matched: u64,
    next_offset: Option<u64>,
}

pub(crate) fn read_evidence_page(
    directory: &Path,
    binding: &Binding,
    receipt: &Receipt,
    offset: u64,
    limit: usize,
    problems_only: bool,
) -> Result<EvidencePage, String> {
    if !(1..=100).contains(&limit) {
        return Err("Choose between 1 and 100 evidence records per page.".into());
    }
    let guarded = windows::guard_runtime(directory)?;
    let mut matched = 0_u64;
    let mut entries = Vec::new();
    let mut bytes = 0;
    let mut page_full = false;
    visit(&guarded, binding, receipt, |record| {
        if problems_only
            && !record.coverage_gap
            && matches!(record.verdict, Verdict::Equal | Verdict::Directory)
        {
            return Ok(());
        }
        if matched >= offset && entries.len() < limit && !page_full {
            let entry = EvidenceEntry {
                relative: record.relative.clone(),
                source_relative: record.source_relative.clone(),
                mirror_relative: record.mirror_relative.clone(),
                verdict: record.verdict,
                directory: record.directory,
                coverage_gap: record.coverage_gap,
                source: record.source.clone(),
                mirror: record.mirror.clone(),
                source_membership: record.source_membership.clone(),
                mirror_membership: record.mirror_membership.clone(),
                problem: record.problem.clone(),
                source_problem: record.source_gap.as_ref().map(|gap| gap.message.clone()),
                mirror_problem: record.mirror_gap.as_ref().map(|gap| gap.message.clone()),
                source_verdict: record.source_gap.as_ref().map(|gap| gap.verdict),
                mirror_verdict: record.mirror_gap.as_ref().map(|gap| gap.verdict),
            };
            let entry_bytes = serde_json::to_vec(&entry).map_err(store::error)?.len();
            page_full = !entries.is_empty() && bytes + entry_bytes > PAGE_BYTES;
            if !page_full {
                entries.push(entry);
                bytes += entry_bytes;
            }
        }
        matched += 1;
        Ok(())
    })?;
    let next = offset.saturating_add(entries.len() as u64);
    Ok(EvidencePage {
        entries,
        matched,
        next_offset: (next < matched).then_some(next),
    })
}

/// Captured by the native service from the instance and two actual watchers.
/// Saved sequences alone do not imply uninterrupted monitoring on reopen.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EvidenceWindow {
    pub(crate) instance_id: String,
    pub(crate) binding_id: String,
    pub(crate) manifest_digest: String,
    pub(crate) source_sequence: u64,
    pub(crate) mirror_sequence: u64,
    pub(crate) source_continuous: bool,
    pub(crate) mirror_continuous: bool,
}

impl EvidenceWindow {
    pub(crate) fn matches_current(
        &self,
        binding: &Binding,
        receipt: &Receipt,
        current: &Self,
    ) -> bool {
        !self.instance_id.is_empty()
            && self.binding_id == binding.binding_id
            && self.manifest_digest == receipt.manifest_digest
            && self.source_continuous
            && self.mirror_continuous
            && self == current
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FrozenMembership {
    pub(crate) schema: u32,
    pub(crate) binding_id: String,
    pub(crate) manifest_digest: String,
    pub(crate) membership_digest: String,
    pub(crate) covered_files: u64,
    pub(crate) covered_directories: u64,
    pub(crate) exceptions: Vec<String>,
}

pub(crate) struct ConfirmationFence<'a> {
    pub(crate) confirmed: bool,
    pub(crate) expected_revision: u64,
    pub(crate) current_revision: u64,
    pub(crate) expected_membership_digest: &'a str,
    pub(crate) verified: &'a EvidenceWindow,
    pub(crate) current: &'a EvidenceWindow,
}

fn update_digest(value: &impl Serialize, digest: &mut Sha256) -> Result<(), String> {
    digest.update(serde_json::to_vec(value).map_err(store::error)?);
    digest.update(b"\n");
    Ok(())
}

pub(crate) fn prepare_membership(
    directory: &Path,
    binding: &Binding,
    receipt: &Receipt,
    exceptions: &[String],
) -> Result<FrozenMembership, String> {
    prepare_and_visit(directory, binding, receipt, exceptions, |_| Ok(()))
}

fn prepare_and_visit(
    directory: &Path,
    binding: &Binding,
    receipt: &Receipt,
    exceptions: &[String],
    mut included: impl FnMut(&Record) -> Result<(), String>,
) -> Result<FrozenMembership, String> {
    if receipt.schema != MANIFEST_SCHEMA {
        return Err("Run a fresh comparison with exact directory membership evidence before confirming this group.".into());
    }
    if exceptions.len() > EXCEPTION_MAX
        || exceptions
            .iter()
            .any(|path| path.len() > 32768 || !windows::relative_supported(path))
    {
        return Err("Review up to 1,000 exact, supported relative exceptions.".into());
    }
    let requested: BTreeSet<_> = exceptions.iter().cloned().collect();
    if requested.len() != exceptions.len() {
        return Err("Reviewed exceptions contain a duplicate relative path.".into());
    }
    let mut found = BTreeSet::new();
    let mut digest = Sha256::new();
    update_digest(
        &(
            "pc-migration-frozen-membership",
            MANIFEST_SCHEMA,
            &binding.binding_id,
            &receipt.manifest_digest,
            &requested,
        ),
        &mut digest,
    )?;
    let mut covered_files = 0;
    let mut covered_directories = 0;
    let mut unresolved = None;
    let guarded = windows::guard_runtime(directory)?;
    visit(&guarded, binding, receipt, |entry| {
        if requested.contains(&entry.relative) {
            found.insert(entry.relative.clone());
        }
        let excluded = requested.contains(&entry.relative)
            || entry
                .relative
                .match_indices('\\')
                .any(|(offset, _)| requested.contains(&entry.relative[..offset]));
        if excluded {
            return Ok(());
        }
        match entry.verdict {
            Verdict::Equal if !entry.coverage_gap => {
                let relative = entry
                    .source_relative
                    .as_deref()
                    .ok_or("Equal file is missing its source identity.")?;
                if entry.source.is_none() || entry.source != entry.mirror {
                    return Err("Equal file has invalid two-sided digest evidence.".into());
                }
                covered_files += 1;
                included(entry)?;
                update_digest(&(relative, "file", &entry.source), &mut digest)?;
            }
            Verdict::Directory if !entry.coverage_gap => {
                let relative = entry
                    .source_relative
                    .as_deref()
                    .ok_or("Equal directory is missing its source identity.")?;
                let source_names = entry
                    .source_membership
                    .as_ref()
                    .ok_or("Source membership witness is missing.")?;
                let mirror_names = entry
                    .mirror_membership
                    .as_ref()
                    .ok_or("Mirror membership witness is missing.")?;
                covered_directories += 1;
                included(entry)?;
                update_digest(
                    &(relative, "directory", source_names, mirror_names),
                    &mut digest,
                )?;
            }
            Verdict::Extra if !entry.coverage_gap => {}
            _ => {
                if unresolved.is_none() {
                    unresolved = Some(if entry.relative.is_empty() {
                        "the selected scope".into()
                    } else {
                        entry.relative.clone()
                    });
                }
            }
        }
        Ok(())
    })?;
    if found != requested {
        return Err("At least one reviewed exception is absent from the saved manifest.".into());
    }
    if let Some(path) = unresolved {
        return Err(format!(
            "Review or exclude the unresolved evidence at {path} before confirming this group."
        ));
    }
    if covered_files == 0 && covered_directories <= 1 {
        return Err("There are no verified descendant entries in this confirmation group.".into());
    }
    Ok(FrozenMembership {
        schema: MANIFEST_SCHEMA,
        binding_id: binding.binding_id.clone(),
        manifest_digest: receipt.manifest_digest.clone(),
        membership_digest: format!("{:x}", digest.finalize()),
        covered_files,
        covered_directories,
        exceptions: requested.into_iter().collect(),
    })
}

/// Reopen guarded paths and compare the recorded member identities and stamps.
/// This is an additional freshness fence, not a replacement for uninterrupted
/// monitoring or a second SHA-256 read of every stream.
pub(crate) fn revalidate_membership(
    directory: &Path,
    binding: &Binding,
    receipt: &Receipt,
    environment: Environment<'_>,
    expected: &FrozenMembership,
    cancel: &AtomicBool,
) -> Result<(), String> {
    scan::check_cancel(cancel)?;
    let (current, source, mirror) =
        windows::open_binding(&binding.source.root, &binding.mirror.root, environment)?;
    if current != *binding {
        return Err("The selected scope or volume identity changed. Review a new binding.".into());
    }
    let guarded = windows::guard_evidence(directory, &source, &mirror)?;
    let mut checker = Checker::new(&guarded)?;
    let actual = prepare_and_visit(directory, binding, receipt, &expected.exceptions, |entry| {
        scan::check_cancel(cancel)?;
        for (side, relative, stamp, witness) in [
            (
                &source,
                entry.source_relative.as_deref(),
                entry.source_stamp.as_ref(),
                entry.source_membership.as_ref(),
            ),
            (
                &mirror,
                entry.mirror_relative.as_deref(),
                entry.mirror_stamp.as_ref(),
                entry.mirror_membership.as_ref(),
            ),
        ] {
            let relative =
                relative.ok_or("The checked member path is missing from the evidence.")?;
            let stamp = stamp.ok_or("The checked member identity is missing from the evidence.")?;
            let gap = if entry.directory {
                let witness = witness
                    .ok_or("The checked directory membership is missing from the evidence.")?;
                checker.recheck(side, relative, stamp, witness, cancel)?
            } else {
                side.recheck_file(relative, stamp).err()
            };
            if let Some(gap) = gap {
                return Err(format!(
                    "Evidence needs reconciliation at {}: {}",
                    entry.relative, gap.message
                ));
            }
        }
        Ok(())
    })?;
    if actual != *expected {
        return Err("The saved frozen membership changed. Prepare the confirmation again.".into());
    }
    super::revalidate_binding(binding, environment)
}

pub(crate) fn confirm_membership(
    directory: &Path,
    binding: &Binding,
    receipt: &Receipt,
    exceptions: &[String],
    fence: &ConfirmationFence<'_>,
) -> Result<FrozenMembership, String> {
    if !fence.confirmed {
        return Err("Grouped manual confirmation requires explicit user confirmation.".into());
    }
    if fence.expected_revision != fence.current_revision {
        return Err("The saved review changed. Prepare the grouped confirmation again.".into());
    }
    if !fence
        .verified
        .matches_current(binding, receipt, fence.current)
    {
        return Err(
            "Mirror evidence is historical or monitoring changed. Reconcile before confirming."
                .into(),
        );
    }
    let membership = prepare_membership(directory, binding, receipt, exceptions)?;
    if membership.membership_digest != fence.expected_membership_digest {
        return Err(
            "Reviewed membership or exceptions changed. Prepare the confirmation again.".into(),
        );
    }
    Ok(membership)
}
