//! Read-only comparison of an explicitly selected source and mirror scope.
//! A receipt describes dated observations. Saved marks and copy permissions
//! are owned by the caller and are never changed here.
mod manifest;
mod membership;
mod scan;
mod source;
mod windows;

use crate::{filesystem::Volume, store};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

pub(crate) use manifest::{
    check_receipt, confirm_membership, prepare_membership, publish_evidence_file,
    read_evidence_file, read_evidence_page, revalidate_membership, ConfirmationFence, EvidencePage,
    EvidenceWindow, FrozenMembership,
};
pub(crate) use source::{
    create_source_evidence_directory, freeze_source, revalidate_source_scope, SourceCandidate,
    SourceEnvironment, SourceProgress, SourceReceipt,
};
#[cfg(test)]
mod tests;

const SCHEMA: u32 = 1;

#[derive(Clone, Copy)]
pub(crate) enum Environment<'a> {
    Live {
        review_volumes: &'a [Volume],
    },
    /// These two paths come from app-owned fixture configuration, never IPC.
    Fixture {
        source_root: &'a Path,
        mirror_root: &'a Path,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Endpoint {
    pub(crate) root: String,
    pub(crate) volume_root: String,
    pub(crate) volume_id: String,
    pub(crate) root_identity: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Binding {
    pub(crate) schema: u32,
    pub(crate) binding_id: String,
    pub(crate) source: Endpoint,
    pub(crate) mirror: Endpoint,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Verdict {
    Equal,
    Directory,
    Missing,
    Different,
    Extra,
    ChangedDuringRead,
    Unreadable,
    Unsupported,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Counts {
    pub(crate) equal: u64,
    pub(crate) directories: u64,
    pub(crate) missing: u64,
    pub(crate) different: u64,
    pub(crate) extra: u64,
    pub(crate) changed_during_read: u64,
    pub(crate) unreadable: u64,
    pub(crate) unsupported: u64,
    pub(crate) coverage_gaps: u64,
}

impl Counts {
    fn record(&mut self, verdict: Verdict) {
        let count = match verdict {
            Verdict::Equal => &mut self.equal,
            Verdict::Directory => &mut self.directories,
            Verdict::Missing => &mut self.missing,
            Verdict::Different => &mut self.different,
            Verdict::Extra => &mut self.extra,
            Verdict::ChangedDuringRead => &mut self.changed_during_read,
            Verdict::Unreadable => &mut self.unreadable,
            Verdict::Unsupported => &mut self.unsupported,
        };
        *count += 1;
    }

    pub(crate) fn coverage_complete(&self) -> bool {
        self.coverage_gaps == 0
    }

    pub(crate) fn source_matches(&self) -> bool {
        self.coverage_complete() && self.missing == 0 && self.different == 0
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Progress {
    pub(crate) phase: &'static str,
    pub(crate) counts: Counts,
    pub(crate) checked_files: u64,
    pub(crate) checked_bytes: u64,
    pub(crate) queued_directories: u64,
    pub(crate) queued_files: u64,
    pub(crate) total_known: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Receipt {
    pub(crate) schema: u32,
    pub(crate) manifest_digest: String,
    pub(crate) binding_id: String,
    pub(crate) started_at: String,
    pub(crate) finished_at: String,
    pub(crate) counts: Counts,
    pub(crate) checked_files: u64,
    pub(crate) checked_bytes: u64,
    pub(crate) coverage_complete: bool,
    pub(crate) source_matches: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Digest {
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    relative: String,
    source_relative: Option<String>,
    mirror_relative: Option<String>,
    verdict: Verdict,
    directory: bool,
    coverage_gap: bool,
    source: Option<Digest>,
    mirror: Option<Digest>,
    source_stamp: Option<windows::Stamp>,
    mirror_stamp: Option<windows::Stamp>,
    #[serde(default)]
    source_membership: Option<membership::Witness>,
    #[serde(default)]
    mirror_membership: Option<membership::Witness>,
    problem: Option<String>,
    source_gap: Option<windows::Gap>,
    mirror_gap: Option<windows::Gap>,
}

pub(crate) fn bind(
    source_path: &str,
    mirror_path: &str,
    environment: Environment<'_>,
) -> Result<Binding, String> {
    windows::open_binding(source_path, mirror_path, environment).map(|(binding, _, _)| binding)
}

pub(crate) fn revalidate_binding(
    binding: &Binding,
    environment: Environment<'_>,
) -> Result<(), String> {
    if bind(&binding.source.root, &binding.mirror.root, environment)? != *binding {
        return Err("The selected scope or volume identity changed. Review a new binding.".into());
    }
    Ok(())
}

pub(crate) fn create_mirror_evidence_directory(
    parent: &Path,
    name: &str,
    binding: &Binding,
    environment: Environment<'_>,
) -> Result<PathBuf, String> {
    let (source, mirror) = checked_binding(binding, environment)?;
    windows::create_evidence_directory(parent, name, &[&source, &mirror])
}

fn checked_binding(
    binding: &Binding,
    environment: Environment<'_>,
) -> Result<(windows::Side, windows::Side), String> {
    let (current, source, mirror) =
        windows::open_binding(&binding.source.root, &binding.mirror.root, environment)?;
    if current != *binding {
        return Err("The selected scope or volume identity changed. Review a new binding.".into());
    }
    Ok((source, mirror))
}

pub(crate) fn verify(
    binding: &Binding,
    environment: Environment<'_>,
    manifest_dir: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(&Progress),
) -> Result<Receipt, String> {
    let started_at = store::now();
    scan::check_cancel(cancel)?;
    let (source, mirror) = checked_binding(binding, environment)?;
    let directory = windows::guard_evidence(manifest_dir, &source, &mirror)?;
    scan::run(
        binding,
        &source,
        &mirror,
        &directory,
        cancel,
        started_at,
        progress,
        || revalidate_binding(binding, environment),
    )
}
