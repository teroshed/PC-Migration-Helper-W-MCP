use crate::store::{error, hash};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub root: String,
    pub id: String,
    pub label: String,
    pub available: bool,
    pub problem: Option<String>,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub size: u64,
    pub modified: String,
    pub fingerprint: String,
    pub problem: Option<String>,
}

pub fn volumes(fixture: Option<&Path>) -> Vec<Volume> {
    if let Some(root) = fixture {
        return vec![Volume {
            root: root.to_string_lossy().into(),
            id: "fixture-volume".into(),
            label: "Safe test folders".into(),
            available: true,
            problem: None,
        }];
    }
    [r"C:\", r"E:\"].iter().map(|root| volume(root)).collect()
}
fn volume(root: &str) -> Volume {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    };
    let wide: Vec<u16> = Path::new(root)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut guid = [0u16; 128];
    let mut label = [0u16; 261];
    let mut serial = 0;
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(wide.as_ptr(), guid.as_mut_ptr(), guid.len() as u32)
    };
    if ok == 0 {
        return Volume {
            root: root.into(),
            id: format!("unavailable-{root}"),
            label: root.into(),
            available: false,
            problem: Some(error(std::io::Error::last_os_error())),
        };
    }
    unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            label.as_mut_ptr(),
            261,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    let decode = |a: &[u16]| {
        String::from_utf16_lossy(&a[..a.iter().position(|v| *v == 0).unwrap_or(a.len())])
    };
    Volume {
        root: root.into(),
        id: format!("{}:{serial:08x}", decode(&guid)),
        label: decode(&label),
        available: true,
        problem: None,
    }
}
pub fn normalize(path: &str) -> Result<String, String> {
    let p = path.replace('/', "\\");
    if p.len() < 3
        || p.as_bytes()[1] != b':'
        || p.as_bytes()[2] != b'\\'
        || !p.as_bytes()[0].is_ascii_alphabetic()
    {
        return Err("Enter an absolute local Windows path.".into());
    }
    if p[2..].contains(':')
        || p.split('\\')
            .any(|s| s == ".." || s == "." || s.ends_with(' ') || s.ends_with('.'))
    {
        return Err(
            "Relative segments, streams and ambiguous Windows paths are not supported.".into(),
        );
    }
    Ok(if p.len() == 3 {
        p
    } else {
        p.trim_end_matches('\\').into()
    })
}
pub fn under(path: &str, root: &str) -> bool {
    let p = path.to_lowercase();
    let r = root.trim_end_matches('\\').to_lowercase();
    p == r || p.starts_with(&(r + "\\"))
}
pub fn identify<'a>(
    path: &str,
    volumes: &'a [Volume],
) -> Result<(&'a Volume, String, String), String> {
    let result = locate(path, volumes)?;
    let v = result.0;
    if !v.available {
        return Err("This volume is unavailable. Existing decisions remain retained.".into());
    }
    if v.id != "fixture-volume" && volume(&v.root).id != v.id {
        return Err("The volume at this drive letter changed. Reopen the app before continuing; old decisions will not be applied to another disk.".into());
    }
    Ok(result)
}
pub fn locate<'a>(
    path: &str,
    volumes: &'a [Volume],
) -> Result<(&'a Volume, String, String), String> {
    let p = normalize(path)?;
    let v = volumes
        .iter()
        .find(|v| under(&p, &v.root))
        .ok_or("This path is outside the configured review roots.")?;
    let relative = p[v.root.trim_end_matches('\\').len()..]
        .trim_start_matches('\\')
        .to_string();
    let key = format!("{}|{}", v.id, relative.to_lowercase());
    Ok((v, relative, key))
}
pub fn safe(path: &str, volumes: &[Volume], allow_final_link: bool) -> Result<PathBuf, String> {
    guard_path(path, volumes, allow_final_link).map(|(path, _)| path)
}
// Hold handles to the ancestors, opened without following the final reparse point.
// Omitting delete-sharing prevents replacement/rename while the caller enumerates.
pub fn guard_path(
    path: &str,
    volumes: &[Volume],
    allow_final_link: bool,
) -> Result<(PathBuf, Vec<fs::File>), String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let (v, _, _) = identify(path, volumes)?;
    let p = PathBuf::from(normalize(path)?);
    let mut parts = Vec::new();
    let mut current = Some(p.as_path());
    while let Some(part) = current {
        if !under(&part.to_string_lossy(), &v.root) {
            break;
        }
        parts.push(part.to_path_buf());
        current = part.parent();
    }
    let mut guards = Vec::new();
    for part in parts.into_iter().rev() {
        let f = fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&part)
            .map_err(|e| format!("{}: {e}", part.display()))?;
        let m = f.metadata().map_err(error)?;
        if reparse(&m) && !(allow_final_link && part == p) {
            return Err(format!(
                "Link/reparse point not followed: {}. Review its target separately.",
                part.display()
            ));
        }
        guards.push(f);
    }
    Ok((p, guards))
}
pub fn reparse(m: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & 0x400 != 0
}
pub fn inspect(path: &Path) -> Entry {
    let text = path.to_string_lossy().to_string();
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| text.clone());
    match fs::symlink_metadata(path) {
        Ok(m) => {
            let kind = if reparse(&m) {
                "link"
            } else if m.is_dir() {
                "folder"
            } else {
                "file"
            };
            let raw_time = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos().to_string())
                .unwrap_or_default();
            let modified = m
                .modified()
                .ok()
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
                .unwrap_or_default();
            let size = if m.is_file() { m.len() } else { 0 };
            let fingerprint = hash(format!("{kind}|{size}|{raw_time}").as_bytes());
            Entry {
                path: text,
                name,
                kind: kind.into(),
                size,
                modified,
                fingerprint,
                problem: None,
            }
        }
        Err(e) => Entry {
            path: text,
            name,
            kind: "unavailable".into(),
            size: 0,
            modified: String::new(),
            fingerprint: String::new(),
            problem: Some(error(e)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fences_paths() {
        assert!(!under(r"E:\foo-other", r"E:\foo"));
        assert!(under(r"E:\foo\bar", r"e:\FOO"));
        assert!(normalize(r"E:\foo\..\bar").is_err());
        assert!(normalize(r"E:\foo:secret").is_err());
        assert!(normalize(r"\\server\share").is_err());
    }
}
