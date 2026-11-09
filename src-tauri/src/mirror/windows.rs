use super::{scan, Binding, Digest, Endpoint, Environment, Verdict, SCHEMA};
use crate::{audit::handle_identity, copy_queue, filesystem, store};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    ffi::OsStr,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read},
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use windows_sys::Win32::{
    Foundation::{ERROR_HANDLE_EOF, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        FileBasicInfo, FileStandardInfo, FindClose, FindFirstStreamW, FindNextStreamW,
        FindStreamInfoStandard, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
        GetVolumeInformationByHandleW, FILE_ATTRIBUTE_OFFLINE,
        FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_BASIC_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_FLAG_SEQUENTIAL_SCAN, FILE_GENERIC_READ, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, VOLUME_NAME_DOS, VOLUME_NAME_GUID,
        WIN32_FIND_STREAM_DATA,
    },
};

// The flag is returned by GetVolumeInformationByHandleW. windows-sys exposes
// FILE_NAMED_STREAMS in a separate SystemServices feature.
const NAMED_STREAMS: u32 = 0x0004_0000;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct Stamp {
    identity: String,
    kind: Kind,
    size: u64,
    written: i64,
    created: i64,
    changed: i64,
    attributes: u32,
}

impl Stamp {
    pub(super) fn kind(&self) -> Kind {
        self.kind
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Gap {
    pub(super) verdict: Verdict,
    pub(super) message: String,
}

impl Gap {
    pub(super) fn changed(message: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::ChangedDuringRead,
            message: message.into(),
        }
    }

    pub(super) fn unsupported(message: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Unsupported,
            message: message.into(),
        }
    }

    pub(super) fn unreadable(message: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Unreadable,
            message: message.into(),
        }
    }
}

pub(super) struct Side {
    path: PathBuf,
    stamp: Stamp,
    streams: bool,
    _guards: Vec<File>,
}

pub(super) struct EvidenceDirectory {
    pub(super) path: PathBuf,
    _guards: Vec<File>,
}

fn compatible_component(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with([' ', '.'])
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '<' | '>' | '"' | ':' | '/' | '\\' | '|' | '?' | '*')
        })
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

pub(super) fn display_component(name: &OsStr) -> (String, bool) {
    if let Some(name) = name.to_str().filter(|name| compatible_component(name)) {
        return (name.into(), true);
    }
    let units = name
        .encode_wide()
        .map(|part| format!("{part:04x}"))
        .collect::<String>();
    (format!("<unsupported-{units}>"), false)
}

pub(super) fn relative_supported(path: &str) -> bool {
    !path.is_empty() && path.split('\\').all(compatible_component)
}

fn normalize_absolute(path: &str) -> Result<String, String> {
    let normalized = filesystem::normalize(path)?;
    if normalized.len() < 3 || (normalized.len() > 3 && !relative_supported(&normalized[3..])) {
        return Err("The selected scope contains unsupported Windows path components.".into());
    }
    Ok(normalized)
}

fn eligible(metadata: &Metadata) -> Result<(), Gap> {
    if filesystem::reparse(metadata)
        || metadata.file_attributes()
            & (FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
    {
        return Err(Gap::unsupported(
            "Link, reparse point or offline data was skipped without hydration.",
        ));
    }
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(Gap::unsupported(
            "This is not a supported regular file or directory.",
        ));
    }
    Ok(())
}

fn open(path: &Path, access: u32, share: u32, extra: u32) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(access)
        .share_mode(share)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS
                | FILE_FLAG_OPEN_REPARSE_POINT
                | FILE_FLAG_OPEN_NO_RECALL
                | extra,
        )
        .open(path)
}

fn open_error(error: io::Error, expected: bool) -> Gap {
    if expected && error.kind() == io::ErrorKind::NotFound {
        Gap::changed("A previously observed member no longer opens.")
    } else {
        Gap::unreadable(error.to_string())
    }
}

fn directory_guard(path: &Path, expected: bool) -> Result<File, Gap> {
    let file = open(
        path,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        0,
    )
    .map_err(|error| open_error(error, expected))?;
    let metadata = file
        .metadata()
        .map_err(|error| open_error(error, expected))?;
    eligible(&metadata)?;
    if !metadata.is_dir() {
        return Err(Gap::changed(
            "A previously expected directory is a different entry type.",
        ));
    }
    Ok(file)
}

fn stamp(file: &File) -> Result<Stamp, Gap> {
    let metadata = file.metadata().map_err(|error| open_error(error, true))?;
    eligible(&metadata)?;
    let mut basic = FILE_BASIC_INFO::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(Gap::unreadable(io::Error::last_os_error().to_string()));
    }
    Ok(Stamp {
        identity: handle_identity(file).map_err(Gap::unreadable)?,
        kind: if metadata.is_dir() {
            Kind::Directory
        } else {
            Kind::File
        },
        size: if metadata.is_dir() { 0 } else { metadata.len() },
        written: basic.LastWriteTime,
        created: basic.CreationTime,
        changed: basic.ChangeTime,
        attributes: basic.FileAttributes,
    })
}

fn path_text(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| "An absolute scope path has unsupported characters.".into())
}

fn final_path(file: &File, volume_name: u32) -> Result<String, String> {
    let mut name = vec![0_u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            name.as_mut_ptr(),
            name.len() as u32,
            volume_name,
        )
    } as usize;
    if length == 0 || length >= name.len() {
        return Err("Could not validate the opened local directory path.".into());
    }
    String::from_utf16(&name[..length]).map_err(store::error)
}

fn canonical_dos(file: &File) -> Result<String, String> {
    let raw = final_path(file, VOLUME_NAME_DOS)?;
    normalize_absolute(raw.strip_prefix(r"\\?\").unwrap_or(&raw))
}

fn check_opened_spelling(path: &str, file: &File) -> Result<String, String> {
    let opened = canonical_dos(file)?;
    if opened.to_lowercase() != path.to_lowercase() {
        return Err("A short name, drive alias or ambiguous path was resolved. Select the opened directory by its actual absolute path.".into());
    }
    Ok(opened)
}

fn named_streams(file: &File) -> Result<bool, String> {
    let mut flags = 0;
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Err(format!(
            "Cannot determine the selected volume's stream support: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(flags & NAMED_STREAMS != 0)
}

fn guards_for_absolute(path: &Path) -> Result<Vec<File>, String> {
    let parts: Vec<_> = path
        .ancestors()
        .take_while(|part| part.has_root())
        .collect();
    parts
        .into_iter()
        .rev()
        .map(|part| {
            directory_guard(part, false)
                .map_err(|gap| format!("{}: {}", part.display(), gap.message))
        })
        .collect()
}

fn fixture_scope(path: &str, anchor: &Path, role: &str) -> Result<(String, String), String> {
    let normalized = normalize_absolute(path_text(anchor)?)?;
    if anchor.file_name() != Some(OsStr::new(role)) || !filesystem::under(path, &normalized) {
        return Err(
            "Fixture comparison must stay under its generated source and mirror folders.".into(),
        );
    }
    let guards = guards_for_absolute(anchor)?;
    let anchor = guards.last().ok_or("Missing fixture anchor handle")?;
    let identity = handle_identity(anchor)?;
    Ok((
        normalized,
        format!("fixture-{role}:{}", store::hash(identity.as_bytes())),
    ))
}

fn side(
    path: &str,
    volume: Option<&filesystem::Volume>,
    fixture: Option<(&Path, &str)>,
) -> Result<(Endpoint, Side), String> {
    let guards = guards_for_absolute(Path::new(path))?;
    let directory = guards.last().ok_or("Missing scope directory handle")?;
    let root = check_opened_spelling(path, directory)?;
    let (volume_root, volume_id, operation_path) = if let Some(volume) = volume {
        if !volume.available {
            return Err(volume
                .problem
                .clone()
                .unwrap_or_else(|| "The selected volume is unavailable.".into()));
        }
        copy_queue::verify_source_volume(volume, &guards)?;
        (
            volume.root.clone(),
            volume.id.clone(),
            PathBuf::from(final_path(directory, VOLUME_NAME_GUID)?),
        )
    } else {
        let (anchor, role) = fixture.ok_or("Missing fixture volume configuration")?;
        let (root_volume, identity) = fixture_scope(&root, anchor, role)?;
        (root_volume, identity, PathBuf::from(&root))
    };
    let observed = stamp(directory).map_err(|gap| gap.message)?;
    let streams = named_streams(directory)?;
    Ok((
        Endpoint {
            root,
            volume_root,
            volume_id,
            root_identity: observed.identity.clone(),
        },
        Side {
            path: operation_path,
            stamp: observed,
            streams,
            _guards: guards,
        },
    ))
}

pub(super) fn open_binding(
    source_path: &str,
    mirror_path: &str,
    environment: Environment<'_>,
) -> Result<(Binding, Side, Side), String> {
    let source_path = normalize_absolute(source_path)?;
    let mirror_path = normalize_absolute(mirror_path)?;
    if filesystem::under(&source_path, &mirror_path)
        || filesystem::under(&mirror_path, &source_path)
    {
        return Err("Choose separate, non-overlapping source and mirror folders.".into());
    }
    let ((source_endpoint, source), (mirror_endpoint, mirror)) = match environment {
        Environment::Live { review_volumes } => {
            let (source_volume, _, _) = filesystem::identify(&source_path, review_volumes)?;
            let mirror_volume = filesystem::volume(&mirror_path[..3]);
            if source_volume.id.eq_ignore_ascii_case(&mirror_volume.id) {
                return Err("The mirror must be on a different Windows volume.".into());
            }
            (
                side(&source_path, Some(source_volume), None)?,
                side(&mirror_path, Some(&mirror_volume), None)?,
            )
        }
        Environment::Fixture {
            source_root,
            mirror_root,
        } => {
            if source_root.file_name() != Some(OsStr::new("source"))
                || mirror_root.file_name() != Some(OsStr::new("mirror"))
                || source_root.parent().is_none()
                || source_root.parent() != mirror_root.parent()
            {
                return Err("Fixture source and mirror must be fixed siblings.".into());
            }
            // Validate the allowlist before opening any user-supplied fixture path.
            if !filesystem::under(&source_path, &normalize_absolute(path_text(source_root)?)?)
                || !filesystem::under(&mirror_path, &normalize_absolute(path_text(mirror_root)?)?)
            {
                return Err("Fixture comparison is outside its generated roots.".into());
            }
            (
                side(&source_path, None, Some((source_root, "source")))?,
                side(&mirror_path, None, Some((mirror_root, "mirror")))?,
            )
        }
    };
    if source_endpoint
        .volume_id
        .eq_ignore_ascii_case(&mirror_endpoint.volume_id)
        || source_endpoint.root_identity == mirror_endpoint.root_identity
    {
        return Err("Source and mirror resolved to the same volume or directory.".into());
    }
    let binding_id = store::hash(
        &serde_json::to_vec(&(SCHEMA, &source_endpoint, &mirror_endpoint)).map_err(store::error)?,
    );
    Ok((
        Binding {
            schema: SCHEMA,
            binding_id,
            source: source_endpoint,
            mirror: mirror_endpoint,
        },
        source,
        mirror,
    ))
}

pub(super) fn guard_runtime(path: &Path) -> Result<EvidenceDirectory, String> {
    let normalized = normalize_absolute(path_text(path)?)?;
    let guards = guards_for_absolute(Path::new(&normalized))?;
    let directory = guards.last().ok_or("Missing evidence directory handle")?;
    let canonical = check_opened_spelling(&normalized, directory)?;
    Ok(EvidenceDirectory {
        path: PathBuf::from(canonical),
        _guards: guards,
    })
}

pub(super) fn guard_evidence(
    path: &Path,
    source: &Side,
    mirror: &Side,
) -> Result<EvidenceDirectory, String> {
    let output = guard_runtime(path)?;
    let canonical = path_text(&output.path)?;
    let directory = output
        ._guards
        .last()
        .ok_or("Missing evidence directory handle")?;
    let guid = final_path(directory, VOLUME_NAME_GUID)?;
    check_outside(canonical, &guid, source, mirror)?;
    Ok(output)
}

fn check_outside(canonical: &str, guid: &str, source: &Side, mirror: &Side) -> Result<(), String> {
    if [source, mirror].iter().any(|side| {
        let scope = side.path.to_string_lossy();
        filesystem::under(canonical, &scope) || filesystem::under(guid, &scope)
    }) {
        return Err("Use an app evidence directory outside both compared scopes.".into());
    }
    Ok(())
}

pub(super) fn create_evidence_directory(
    parent: &Path,
    name: &str,
    source: &Side,
    mirror: &Side,
) -> Result<PathBuf, String> {
    if !compatible_component(name) || name.len() > 255 {
        return Err("An evidence directory name must be one supported path component.".into());
    }
    let guarded = guard_runtime(parent)?;
    let parent = guarded
        ._guards
        .last()
        .ok_or("Missing evidence parent handle")?;
    let target = guarded.path.join(name);
    let guid_target = PathBuf::from(final_path(parent, VOLUME_NAME_GUID)?).join(name);
    check_outside(
        path_text(&target)?,
        path_text(&guid_target)?,
        source,
        mirror,
    )?;
    if let Err(error) = fs::create_dir(&target) {
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(store::error(error));
        }
    }
    Ok(guard_evidence(&target, source, mirror)?.path)
}

pub(super) fn open_evidence(path: &Path) -> Result<File, String> {
    let file = open(
        path,
        FILE_GENERIC_READ,
        FILE_SHARE_READ,
        FILE_FLAG_SEQUENTIAL_SCAN,
    )
    .map_err(store::error)?;
    let metadata = file.metadata().map_err(store::error)?;
    eligible(&metadata).map_err(|gap| gap.message)?;
    if !metadata.is_file() {
        return Err("The evidence path is not a regular local file.".into());
    }
    let mut standard = FILE_STANDARD_INFO::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&mut standard as *mut FILE_STANDARD_INFO).cast(),
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if ok == 0 || standard.NumberOfLinks != 1 {
        return Err("The evidence sidecar must have a unique, guarded file name.".into());
    }
    Ok(file)
}

fn wide(path: &Path) -> Vec<u16> {
    let mut result = Vec::new();
    if !path.to_string_lossy().starts_with(r"\\?\") {
        result.extend(r"\\?\".encode_utf16());
    }
    result.extend(path.as_os_str().encode_wide());
    result.push(0);
    result
}

pub(super) fn wide_path(path: &Path) -> Vec<u16> {
    wide(path)
}

fn check_streams(path: &Path, supported: bool) -> Result<(), Gap> {
    if !supported {
        return Ok(());
    }
    let mut data = WIN32_FIND_STREAM_DATA::default();
    let handle = unsafe {
        FindFirstStreamW(
            wide(path).as_ptr(),
            FindStreamInfoStandard,
            (&mut data as *mut WIN32_FIND_STREAM_DATA).cast(),
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
            return Ok(());
        }
        return Err(Gap::unsupported(format!(
            "Could not check named streams: {error}"
        )));
    }
    let length = data
        .cStreamName
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(data.cStreamName.len());
    let default = data.cStreamName[..length]
        .iter()
        .copied()
        .eq("::$DATA".encode_utf16());
    let next =
        unsafe { FindNextStreamW(handle, (&mut data as *mut WIN32_FIND_STREAM_DATA).cast()) };
    let ended =
        next == 0 && io::Error::last_os_error().raw_os_error() == Some(ERROR_HANDLE_EOF as i32);
    unsafe { FindClose(handle) };
    if default && ended {
        Ok(())
    } else {
        Err(Gap::unsupported(
            "Named or uninspectable streams are outside unnamed-stream verification.",
        ))
    }
}

impl Side {
    pub(super) fn root_stamp(&self) -> Stamp {
        self.stamp.clone()
    }

    fn absolute(&self, relative: &str) -> Result<PathBuf, Gap> {
        if relative.is_empty() {
            return Ok(self.path.clone());
        }
        if !relative_supported(relative) {
            return Err(Gap::unsupported(
                "The relative path has an unsupported component.",
            ));
        }
        Ok(self.path.join(relative))
    }

    fn hold_parents(&self, relative: &str) -> Result<Vec<File>, Gap> {
        let mut current = self.path.clone();
        let mut guards = Vec::new();
        if let Some((parent, _)) = relative.rsplit_once('\\') {
            for part in parent.split('\\') {
                current.push(part);
                guards.push(directory_guard(&current, true)?);
            }
        }
        Ok(guards)
    }

    pub(super) fn observed(path: &Path) -> Result<Stamp, Gap> {
        let file = open(
            path,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            0,
        )
        .map_err(|error| open_error(error, true))?;
        stamp(&file)
    }

    pub(super) fn enumerate(
        &self,
        relative: &str,
        expected: &Stamp,
        cancel: &AtomicBool,
        mut member: impl FnMut(String, Result<Stamp, Gap>) -> Result<(), String>,
    ) -> Result<Result<(), Gap>, String> {
        self.visit_directory(relative, expected, cancel, |entry| {
            let (name, supported) = display_component(&entry.file_name());
            let observed = if supported {
                Self::observed(&entry.path())
            } else {
                Err(Gap::unsupported(
                    "An entry name cannot be represented as a safe Windows relative path.",
                ))
            };
            member(name, observed)
        })
    }

    pub(super) fn enumerate_names(
        &self,
        relative: &str,
        expected: &Stamp,
        cancel: &AtomicBool,
        mut member: impl FnMut(String) -> Result<(), String>,
    ) -> Result<Result<(), Gap>, String> {
        self.visit_directory(relative, expected, cancel, |entry| {
            member(display_component(&entry.file_name()).0)
        })
    }

    fn visit_directory(
        &self,
        relative: &str,
        expected: &Stamp,
        cancel: &AtomicBool,
        mut visit: impl FnMut(fs::DirEntry) -> Result<(), String>,
    ) -> Result<Result<(), Gap>, String> {
        let opened = (|| {
            let path = self.absolute(relative)?;
            let parents = self.hold_parents(relative)?;
            let directory = directory_guard(&path, true)?;
            if &stamp(&directory)? != expected {
                return Err(Gap::changed(
                    "Directory identity or metadata changed before enumeration.",
                ));
            }
            check_streams(&path, self.streams)?;
            let entries = fs::read_dir(&path).map_err(|error| open_error(error, true))?;
            Ok((path, parents, directory, entries))
        })();
        let (path, _parents, directory, entries) = match opened {
            Ok(opened) => opened,
            Err(gap) => return Ok(Err(gap)),
        };
        for entry in entries {
            scan::check_cancel(cancel)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return Ok(Err(open_error(error, true))),
            };
            visit(entry)?;
        }
        Ok((|| {
            if &stamp(&directory)? != expected {
                return Err(Gap::changed(
                    "Directory identity or membership changed during enumeration.",
                ));
            }
            check_streams(&path, self.streams)
        })())
    }

    pub(super) fn recheck_file(&self, relative: &str, expected: &Stamp) -> Result<(), Gap> {
        let path = self.absolute(relative)?;
        let _parents = self.hold_parents(relative)?;
        let file = open(&path, FILE_GENERIC_READ, FILE_SHARE_READ, 0)
            .map_err(|error| open_error(error, true))?;
        if &stamp(&file)? != expected || expected.kind != Kind::File {
            return Err(Gap::changed(
                "The checked file identity or metadata changed.",
            ));
        }
        check_streams(&path, self.streams)
    }

    pub(super) fn hash(
        &self,
        relative: &str,
        expected: &Stamp,
        cancel: &AtomicBool,
        buffer: &mut [u8],
    ) -> Result<Result<Digest, Gap>, String> {
        scan::check_cancel(cancel)?;
        let opened = (|| {
            let path = self.absolute(relative)?;
            let parents = self.hold_parents(relative)?;
            let file = open(
                &path,
                FILE_GENERIC_READ,
                FILE_SHARE_READ,
                FILE_FLAG_SEQUENTIAL_SCAN,
            )
            .map_err(|error| open_error(error, true))?;
            if &stamp(&file)? != expected || expected.kind != Kind::File {
                return Err(Gap::changed(
                    "File identity or metadata changed after enumeration.",
                ));
            }
            check_streams(&path, self.streams)?;
            Ok((path, parents, file))
        })();
        let (path, _parents, mut file) = match opened {
            Ok(opened) => opened,
            Err(gap) => return Ok(Err(gap)),
        };
        let mut digest = Sha256::new();
        let mut bytes = 0;
        loop {
            scan::check_cancel(cancel)?;
            let count = match file.read(buffer) {
                Ok(count) => count,
                Err(error) => return Ok(Err(open_error(error, true))),
            };
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
            bytes += count as u64;
        }
        Ok((|| {
            check_streams(&path, self.streams)?;
            if &stamp(&file)? != expected || bytes != expected.size {
                return Err(Gap::changed(
                    "File identity, size or metadata changed during hashing.",
                ));
            }
            Ok(Digest {
                sha256: format!("{:x}", digest.finalize()),
                bytes,
            })
        })())
    }
}
