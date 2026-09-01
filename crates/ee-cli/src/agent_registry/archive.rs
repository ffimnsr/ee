use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use tar::Archive;
use url::Url;
use zip::ZipArchive;

use super::{ArchiveFormat, MAX_ARCHIVE_ENTRIES, MAX_EXTRACTED_BYTES};

pub(super) fn classify_archive(archive_url: &str) -> Result<ArchiveFormat, String> {
    let url =
        Url::parse(archive_url).map_err(|error| format!("invalid agent archive URL: {error}"))?;
    let name = url.path_segments().and_then(Iterator::last).unwrap_or("").to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Ok(ArchiveFormat::TarGz)
    } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
        Ok(ArchiveFormat::TarBz2)
    } else if name.ends_with(".zip") {
        Ok(ArchiveFormat::Zip)
    } else if name.ends_with(".exe") || name.ends_with(".bin") || !name.contains('.') {
        Ok(ArchiveFormat::Raw)
    } else {
        Err(format!("unsupported agent archive format in URL path `{}`", url.path()))
    }
}

pub(super) fn extract_download(
    archive_path: &Path,
    archive_url: &str,
    destination: &Path,
    command: &Path,
) -> Result<(), String> {
    let file =
        File::open(archive_path).map_err(|error| format!("cannot open agent archive: {error}"))?;
    match classify_archive(archive_url)? {
        ArchiveFormat::Zip => extract_zip(file, destination),
        ArchiveFormat::TarGz => extract_tar(GzDecoder::new(file), destination),
        ArchiveFormat::TarBz2 => extract_tar(BzDecoder::new(file), destination),
        ArchiveFormat::Raw => {
            let output = destination.join(command);
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
            fs::copy(archive_path, &output)
                .map_err(|error| format!("cannot install raw agent binary: {error}"))?;
            Ok(())
        }
    }
}

fn extract_zip(file: File, destination: &Path) -> Result<(), String> {
    let mut archive =
        ZipArchive::new(file).map_err(|error| format!("invalid ZIP archive: {error}"))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(String::from("agent archive has too many entries"));
    }
    let mut total = 0_u64;
    let mut paths = BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry =
            archive.by_index(index).map_err(|error| format!("cannot read ZIP entry: {error}"))?;
        let path = safe_relative_path(entry.name())?;
        if !paths.insert(path.clone()) {
            return Err(format!("agent archive repeats path {}", path.display()));
        }
        if entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) {
            return Err(format!("agent archive contains symlink {}", path.display()));
        }
        let output = destination.join(&path);
        if entry.is_dir() {
            fs::create_dir_all(&output)
                .map_err(|error| format!("cannot create {}: {error}", output.display()))?;
            continue;
        }
        let size = entry.size();
        let mode = entry.unix_mode();
        total = total
            .checked_add(size)
            .filter(|total| *total <= MAX_EXTRACTED_BYTES)
            .ok_or_else(|| String::from("agent archive expands beyond size limit"))?;
        write_archive_file(&mut entry, &output, size, mode)?;
    }
    Ok(())
}

fn extract_tar(reader: impl io::Read, destination: &Path) -> Result<(), String> {
    let mut archive = Archive::new(reader);
    let entries = archive.entries().map_err(|error| format!("invalid TAR archive: {error}"))?;
    let mut total = 0_u64;
    let mut count = 0_usize;
    let mut paths = BTreeSet::new();
    for entry in entries {
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err(String::from("agent archive has too many entries"));
        }
        let mut entry = entry.map_err(|error| format!("cannot read TAR entry: {error}"))?;
        let path = safe_relative_path(
            &entry.path().map_err(|error| format!("invalid TAR path: {error}"))?.to_string_lossy(),
        )?;
        if !paths.insert(path.clone()) {
            return Err(format!("agent archive repeats path {}", path.display()));
        }
        let output = destination.join(&path);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            fs::create_dir_all(&output)
                .map_err(|error| format!("cannot create {}: {error}", output.display()))?;
        } else if entry_type.is_file() {
            let size = entry.size();
            let mode = entry.header().mode().ok();
            total = total
                .checked_add(size)
                .filter(|total| *total <= MAX_EXTRACTED_BYTES)
                .ok_or_else(|| String::from("agent archive expands beyond size limit"))?;
            write_archive_file(&mut entry, &output, size, mode)?;
        } else {
            return Err(format!("agent archive contains unsupported entry {}", path.display()));
        }
    }
    Ok(())
}

pub(super) fn write_archive_file(
    reader: &mut impl io::Read,
    output: &Path,
    size: u64,
    source_mode: Option<u32>,
) -> Result<(), String> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| format!("cannot create {}: {error}", output.display()))?;
    let written = io::copy(&mut reader.take(size + 1), &mut file)
        .map_err(|error| format!("cannot extract {}: {error}", output.display()))?;
    if written != size {
        return Err(format!("agent archive entry {} has invalid size", output.display()));
    }
    file.flush().map_err(|error| format!("cannot flush {}: {error}", output.display()))?;
    set_extracted_permissions(output, source_mode)
}

#[cfg(unix)]
fn set_extracted_permissions(path: &Path, source_mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = if source_mode.is_some_and(|mode| mode & 0o111 != 0) { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("cannot secure extracted file {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_extracted_permissions(_path: &Path, _source_mode: Option<u32>) -> Result<(), String> {
    Ok(())
}

pub(super) fn safe_relative_path(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() || raw.starts_with(['/', '\\']) || raw.contains('\0') {
        return Err(format!("unsafe agent archive path `{raw}`"));
    }
    let normalized = raw.replace('\\', "/");
    let mut output = PathBuf::new();
    for component in Path::new(&normalized).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) if safe_path_component(&part.to_string_lossy()) => {
                output.push(part);
            }
            _ => return Err(format!("unsafe agent archive path `{raw}`")),
        }
    }
    if output.as_os_str().is_empty() {
        Err(format!("unsafe agent archive path `{raw}`"))
    } else {
        Ok(output)
    }
}

fn safe_path_component(component: &str) -> bool {
    if component.is_empty() || component.contains(':') || component.ends_with(['.', ' ']) {
        return false;
    }
    let basename = component.split('.').next().unwrap_or(component).to_ascii_uppercase();
    !matches!(basename.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !matches!(basename.strip_prefix("COM"), Some(number) if matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
        && !matches!(basename.strip_prefix("LPT"), Some(number) if matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
}
