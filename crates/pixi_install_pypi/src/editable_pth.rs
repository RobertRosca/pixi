//! Post-processing utilities for editable installation `.pth` files.
//!
//! When a path-based editable PyPI package is installed through a symlink, the build
//! backend (setuptools / hatchling) canonicalises the path before writing it to the
//! `.pth` file.  This module adds the original absolute-but-non-canonical
//! (symlink-preserving) path as a second entry so that both paths work for importing.
//!
//! # Deduplication
//! If the canonical path and the given path are identical (i.e. no symlink in the
//! chain) only a single entry is kept.
//!
//! # Platform scope
//! The feature is Unix-only.  On non-Unix platforms the function is a no-op.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// Normalise a package name to the form used in `.pth` file names.
///
/// Both setuptools and hatchling lower-case the name and replace `-` / `.` with `_`.
pub(crate) fn normalize_pth_name(name: &str) -> String {
    name.to_lowercase().replace(['-', '.'], "_")
}

/// Try to find the editable `.pth` file for a package in `site_packages`.
///
/// Checked naming conventions (in order):
/// * `__editable__.{name}-{version}.pth`  (setuptools)
/// * `_{name}.pth`                         (hatchling)
pub(crate) fn find_editable_pth(site_packages: &Path, normalized_name: &str) -> Option<PathBuf> {
    let entries = fs_err::read_dir(site_packages).ok()?;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.ends_with(".pth") {
            continue;
        }
        // setuptools style: __editable__.{name}-{version}.pth
        // The package name in the file may use either `-` or `_`, so check both.
        let normalized_hyphen = normalized_name.replace('_', "-");
        if name.starts_with(&format!("__editable__.{}-", normalized_name))
            || name.starts_with(&format!("__editable__.{}-", normalized_hyphen))
        {
            return Some(entry.path());
        }
        // hatchling style: _{name}.pth
        if name == format!("_{}.pth", normalized_name) {
            return Some(entry.path());
        }
    }
    None
}

/// Post-process editable `.pth` files so they contain both the canonical path and the
/// absolute-but-non-canonical (symlink-preserving) path.
///
/// For each `(pkg_name, given_path)` pair in `editable_paths`:
///
/// 1. Locate the matching `.pth` file in `site_packages` (setuptools or hatchling naming).
/// 2. Canonicalise `given_path`.
/// 3. If canonical == given (no symlink in chain), do nothing (deduplication).
/// 4. Otherwise ensure the file contains exactly two lines:
///    * line 1: the canonical path
///    * line 2: the given (symlink) path
///
/// The function is idempotent: if the file already has the correct two-line content it
/// is not rewritten.
///
/// # Arguments
/// * `site_packages` – path to the `site-packages` directory to scan.
/// * `editable_paths` – map from (possibly non-normalised) package name to the absolute,
///   non-canonical path as supplied by the user in the manifest.
#[cfg(unix)]
pub fn patch_editable_pth_files(
    site_packages: &Path,
    editable_paths: &HashMap<String, PathBuf>,
) -> io::Result<()> {
    for (pkg_name, given_path) in editable_paths {
        let normalized = normalize_pth_name(pkg_name);
        let Some(pth_path) = find_editable_pth(site_packages, &normalized) else {
            tracing::debug!(
                "no editable .pth file found for {pkg_name} in {}; skipping",
                site_packages.display()
            );
            continue;
        };

        let canonical_path = match dunce::canonicalize(given_path) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(
                    "failed to canonicalize {}: {e}; skipping pth patching for {pkg_name}",
                    given_path.display()
                );
                continue;
            }
        };

        // Paths are identical – nothing to add (deduplication).
        if canonical_path == *given_path {
            tracing::debug!(
                "canonical == given for {pkg_name}, skipping pth patching (deduplication)"
            );
            continue;
        }

        let raw = fs_err::read_to_string(&pth_path)?;
        let existing: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();

        let canonical_str = canonical_path.to_string_lossy();
        let given_str = given_path.to_string_lossy();

        // Already correct – nothing to rewrite.
        if existing.len() >= 2
            && existing[0] == canonical_str.as_ref()
            && existing[1] == given_str.as_ref()
        {
            tracing::debug!(
                "{} already has both paths for {pkg_name}; skipping",
                pth_path.display()
            );
            continue;
        }

        // Write: canonical first, then the symlink path.
        let new_content = format!("{}\n{}\n", canonical_str, given_str);
        fs_err::write(&pth_path, &new_content)?;
        tracing::debug!(
            "patched {}: added symlink path {}",
            pth_path.display(),
            given_str
        );
    }
    Ok(())
}

/// No-op stub on non-Unix platforms.
#[cfg(not(unix))]
pub fn patch_editable_pth_files(
    _site_packages: &Path,
    _editable_paths: &HashMap<String, PathBuf>,
) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Create a setuptools-style editable `.pth` file.
    fn write_setuptools_pth(dir: &Path, pkg_name: &str, version: &str, content: &str) -> PathBuf {
        let path = dir.join(format!("__editable__.{pkg_name}-{version}.pth"));
        fs::write(&path, content).unwrap();
        path
    }

    /// Create a hatchling-style editable `.pth` file.
    fn write_hatchling_pth(dir: &Path, pkg_name: &str, content: &str) -> PathBuf {
        let path = dir.join(format!("_{pkg_name}.pth"));
        fs::write(&path, content).unwrap();
        path
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    fn lines_of(pth: &Path) -> Vec<String> {
        fs::read_to_string(pth)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    }

    // ── tests ─────────────────────────────────────────────────────────────────

    /// When canonical == given (no symlink), the `.pth` file is left with a single line.
    #[test]
    fn test_no_symlink_deduplicated() {
        let site_packages = tempdir().unwrap();
        let pkg_dir = tempdir().unwrap();
        let canonical = dunce::canonicalize(pkg_dir.path()).unwrap();

        let pth = write_setuptools_pth(
            site_packages.path(),
            "mypkg",
            "0.1.0",
            &format!("{}\n", canonical.display()),
        );

        let mut map = HashMap::new();
        map.insert("mypkg".to_string(), canonical.clone());

        patch_editable_pth_files(site_packages.path(), &map).unwrap();

        let lines = lines_of(&pth);
        assert_eq!(lines.len(), 1, "only one line expected; got: {lines:?}");
        assert_eq!(lines[0], canonical.to_str().unwrap());
    }

    /// When canonical != given (symlink), the `.pth` file gets a second line (setuptools).
    #[test]
    fn test_symlink_adds_second_line_setuptools() {
        let site_packages = tempdir().unwrap();
        let real_dir = tempdir().unwrap();
        let symlink_path = site_packages.path().join("pkg_symlink");
        std::os::unix::fs::symlink(real_dir.path(), &symlink_path).unwrap();
        let canonical = dunce::canonicalize(&symlink_path).unwrap();
        assert_ne!(canonical, symlink_path);

        // Start with only the canonical path (as uv/the build backend creates it).
        let pth = write_setuptools_pth(
            site_packages.path(),
            "mypkg",
            "0.1.0",
            &format!("{}\n", canonical.display()),
        );

        let mut map = HashMap::new();
        map.insert("mypkg".to_string(), symlink_path.clone());

        patch_editable_pth_files(site_packages.path(), &map).unwrap();

        let lines = lines_of(&pth);
        assert_eq!(lines.len(), 2, "expected 2 lines; got: {lines:?}");
        assert_eq!(lines[0], canonical.to_str().unwrap(), "line 0 = canonical");
        assert_eq!(
            lines[1],
            symlink_path.to_str().unwrap(),
            "line 1 = symlink"
        );
    }

    /// Same as above but for a hatchling-style `.pth` file.
    #[test]
    fn test_symlink_adds_second_line_hatchling() {
        let site_packages = tempdir().unwrap();
        let real_dir = tempdir().unwrap();
        let symlink_path = site_packages.path().join("pkg_symlink");
        std::os::unix::fs::symlink(real_dir.path(), &symlink_path).unwrap();
        let canonical = dunce::canonicalize(&symlink_path).unwrap();

        let pth = write_hatchling_pth(
            site_packages.path(),
            "mypkg",
            &format!("{}\n", canonical.display()),
        );

        let mut map = HashMap::new();
        map.insert("mypkg".to_string(), symlink_path.clone());

        patch_editable_pth_files(site_packages.path(), &map).unwrap();

        let lines = lines_of(&pth);
        assert_eq!(lines.len(), 2, "expected 2 lines; got: {lines:?}");
        assert_eq!(lines[0], canonical.to_str().unwrap());
        assert_eq!(lines[1], symlink_path.to_str().unwrap());
    }

    /// Calling the function twice must be idempotent (no duplicate lines).
    #[test]
    fn test_idempotent() {
        let site_packages = tempdir().unwrap();
        let real_dir = tempdir().unwrap();
        let symlink_path = site_packages.path().join("pkg_symlink");
        std::os::unix::fs::symlink(real_dir.path(), &symlink_path).unwrap();
        let canonical = dunce::canonicalize(&symlink_path).unwrap();

        let pth = write_setuptools_pth(
            site_packages.path(),
            "mypkg",
            "0.1.0",
            &format!("{}\n", canonical.display()),
        );

        let mut map = HashMap::new();
        map.insert("mypkg".to_string(), symlink_path.clone());

        // Run twice – should still produce exactly 2 lines.
        patch_editable_pth_files(site_packages.path(), &map).unwrap();
        patch_editable_pth_files(site_packages.path(), &map).unwrap();

        let lines = lines_of(&pth);
        assert_eq!(
            lines.len(),
            2,
            "should still be exactly 2 lines after two patches; got: {lines:?}"
        );
    }

    /// Package name with hyphens should still match the normalised `.pth` filename.
    #[test]
    fn test_hyphenated_name_matches() {
        let site_packages = tempdir().unwrap();
        let real_dir = tempdir().unwrap();
        let symlink_path = site_packages.path().join("pkg_symlink");
        std::os::unix::fs::symlink(real_dir.path(), &symlink_path).unwrap();
        let canonical = dunce::canonicalize(&symlink_path).unwrap();

        // setuptools normalises hyphens to underscores in the file name.
        let pth = write_setuptools_pth(
            site_packages.path(),
            "my_pkg",
            "0.1.0",
            &format!("{}\n", canonical.display()),
        );

        let mut map = HashMap::new();
        // Key uses the hyphenated form as it appears in the manifest / lock file.
        map.insert("my-pkg".to_string(), symlink_path.clone());

        patch_editable_pth_files(site_packages.path(), &map).unwrap();

        let lines = lines_of(&pth);
        assert_eq!(lines.len(), 2, "expected 2 lines; got: {lines:?}");
        assert_eq!(lines[1], symlink_path.to_str().unwrap());
    }
}
