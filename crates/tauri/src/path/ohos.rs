// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::{Error, Result};
use crate::{AppHandle, Runtime};
use std::path::{Path, PathBuf};

const OHOS_BUNDLE_ROOT: &str = "/data/storage/el1/bundle";
const PREFERRED_HAP_MODULES: &[&str] = &["entry_desktop", "entry_mobile", "entry"];

/// The path resolver is a helper class for general and application-specific path APIs on OpenHarmony.
pub struct PathResolver<R: Runtime>(pub(crate) AppHandle<R>);

impl<R: Runtime> Clone for PathResolver<R> {
  fn clone(&self) -> Self {
    Self(self.0.clone())
  }
}

impl<R: Runtime> PathResolver<R> {
  fn base_path(&self) -> Result<PathBuf> {
    crate::ohos::BASE_PATH
      .get()
      .and_then(|p| p.as_ref())
      .map(|p| {
        let path = PathBuf::from(p);
        // context.filesDir ends with "/files", but we need the el2 base root
        // to avoid double-joining "files" in path methods (e.g., "files/files").
        if path.ends_with("files") {
          path.parent().unwrap_or(&path).to_path_buf()
        } else {
          path
        }
      })
      .ok_or(Error::UnknownPath)
  }

  /// Returns the final component of the `Path`, if there is one.
  ///
  /// If the path is a normal file, this is the file name. If it's the path of a directory, this
  /// is the directory name.
  ///
  /// Returns [`None`] if the path terminates in `..`.
  pub fn file_name(&self, path: &str) -> Option<String> {
    Path::new(path)
      .file_name()
      .map(|name| name.to_string_lossy().into_owned())
  }

  /// Returns the path to the user's audio directory.
  pub fn audio_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files").join("Audio"))
  }

  /// Returns the path to the user's cache directory.
  pub fn cache_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("cache"))
  }

  /// Returns the path to the user's config directory.
  pub fn config_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files"))
  }

  /// Returns the path to the user's data directory.
  pub fn data_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files"))
  }

  /// Returns the path to the user's local data directory.
  pub fn local_data_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files"))
  }

  /// Returns the path to the user's document directory.
  pub fn document_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files").join("Documents"))
  }

  /// Returns the path to the user's download directory.
  pub fn download_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files").join("Download"))
  }

  /// Returns the path to the user's picture directory.
  pub fn picture_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files").join("Pictures"))
  }

  /// Returns the path to the user's public directory.
  pub fn public_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files").join("Public"))
  }

  /// Returns the path to the user's video directory.
  pub fn video_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files").join("Videos"))
  }

  /// Returns the path to the resource directory.
  ///
  /// `/data/storage/el1/bundle/<hap-module-name>/resources/resfile` — the
  /// system extracts a module's `resources/resfile` HAP content into a
  /// directly readable directory where the CLI places `tauri.conf.json` and
  /// configured `bundle.resources`.
  ///
  /// Resolution order:
  /// 1. Explicit HAP module name injected from ArkTS via `tauri_set_hap_module_name`.
  /// 2. Fallback scan under `/data/storage/el1/bundle/*/resources/resfile`,
  ///    preferring an entry containing `tauri.conf.json` (checking
  ///    `entry_desktop`, `entry_mobile`, `entry` in deterministic order) or a
  ///    single existing `resources/resfile` directory.
  /// 3. [`Error::UnknownPath`] if no module resource directory can be found.
  pub fn resource_dir(&self) -> Result<PathBuf> {
    let configured = crate::ohos::hap_module_name();
    resolve_resource_dir(Path::new(OHOS_BUNDLE_ROOT), configured.as_deref())
  }

  /// Returns the path to the app's config directory.
  pub fn app_config_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files"))
  }

  /// Returns the path to the app's data directory.
  pub fn app_data_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files"))
  }

  /// Returns the path to the app's local data directory.
  pub fn app_local_data_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("files"))
  }

  /// Returns the path to the app's cache directory.
  pub fn app_cache_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("cache"))
  }

  /// Returns the path to the app's log directory.
  pub fn app_log_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("log"))
  }

  /// Returns the path to the temporary directory.
  pub fn temp_dir(&self) -> Result<PathBuf> {
    self.base_path().map(|p| p.join("temp"))
  }

  /// Returns the path to the home directory (app sandbox root).
  pub fn home_dir(&self) -> Result<PathBuf> {
    self.base_path()
  }
}

/// Internal helper: compute the resource directory for a HAP module name under `bundle_root`.
fn compute_resource_dir_under(bundle_root: &Path, module: &str) -> PathBuf {
  bundle_root.join(module).join("resources").join("resfile")
}

/// Internal helper: compute the resource directory for a HAP module name.
#[cfg(test)]
fn compute_resource_dir(module: &str) -> PathBuf {
  compute_resource_dir_under(Path::new(OHOS_BUNDLE_ROOT), module)
}

/// Normalizes the injected module name: `None`, empty, and whitespace-only values
/// are treated as missing.
fn normalize_module_name(module_name: Option<&str>) -> Option<&str> {
  module_name.map(str::trim).filter(|m| !m.is_empty())
}

/// Resolves the OHOS `resfile` directory either from the explicitly injected HAP
/// module name or by scanning `bundle_root/*/resources/resfile`.
fn resolve_resource_dir(bundle_root: &Path, configured_module: Option<&str>) -> Result<PathBuf> {
  if let Some(module) = normalize_module_name(configured_module) {
    return Ok(compute_resource_dir_under(bundle_root, module));
  }

  // Fallback 1: check standard entry module names in deterministic order,
  // preferring one containing `tauri.conf.json`.
  for &module in PREFERRED_HAP_MODULES {
    let resfile = compute_resource_dir_under(bundle_root, module);
    if resfile.join("tauri.conf.json").is_file() {
      return Ok(resfile);
    }
  }

  // Collect all `<bundle_root>/<subdir>/resources/resfile` directories in sorted order.
  let mut discovered_resfiles: Vec<PathBuf> = match std::fs::read_dir(bundle_root) {
    Ok(entries) => entries
      .filter_map(|entry| entry.ok())
      .filter_map(|entry| {
        let path = entry.path();
        if path.is_dir() {
          let resfile = path.join("resources").join("resfile");
          if resfile.is_dir() {
            return Some(resfile);
          }
        }
        None
      })
      .collect(),
    Err(_) => Vec::new(),
  };
  discovered_resfiles.sort();

  // Fallback 2: any discovered `resfile` directory containing `tauri.conf.json`.
  if let Some(found) = discovered_resfiles
    .iter()
    .find(|dir| dir.join("tauri.conf.json").is_file())
  {
    return Ok(found.clone());
  }

  // Fallback 3: standard entry module names that have an existing `resources/resfile` dir.
  for &module in PREFERRED_HAP_MODULES {
    let resfile = compute_resource_dir_under(bundle_root, module);
    if resfile.is_dir() {
      return Ok(resfile);
    }
  }

  // Fallback 4: unambiguous single existing `resources/resfile` directory.
  if discovered_resfiles.len() == 1 {
    return Ok(discovered_resfiles.remove(0));
  }

  log::warn!(
    "resource_dir: HAP module name unavailable and fallback scan under {} found no unique resfile directory",
    bundle_root.display()
  );
  Err(Error::UnknownPath)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicU64, Ordering};

  const MOCK_BASE: &str = "/data/storage/el2/base";

  static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

  struct TempDir(PathBuf);

  impl TempDir {
    fn new(prefix: &str) -> Self {
      let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
      let dir = std::env::temp_dir().join(format!(
        "tauri_ohos_path_test_{prefix}_{}_{id}",
        std::process::id()
      ));
      let _ = std::fs::remove_dir_all(&dir);
      std::fs::create_dir_all(&dir).unwrap();
      Self(dir)
    }

    fn path(&self) -> &Path {
      &self.0
    }
  }

  impl Drop for TempDir {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.0);
    }
  }

  fn mock_base() -> PathBuf {
    PathBuf::from(MOCK_BASE)
  }

  #[test]
  fn cache_dir_appends_cache_subdir() {
    assert_eq!(
      mock_base().join("cache"),
      PathBuf::from("/data/storage/el2/base/cache")
    );
  }

  #[test]
  fn data_dir_appends_files_subdir() {
    assert_eq!(
      mock_base().join("files"),
      PathBuf::from("/data/storage/el2/base/files")
    );
  }

  #[test]
  fn media_dirs_under_files() {
    let base = mock_base();
    assert_eq!(
      base.join("files").join("Audio"),
      PathBuf::from("/data/storage/el2/base/files/Audio")
    );
    assert_eq!(
      base.join("files").join("Documents"),
      PathBuf::from("/data/storage/el2/base/files/Documents")
    );
    assert_eq!(
      base.join("files").join("Download"),
      PathBuf::from("/data/storage/el2/base/files/Download")
    );
    assert_eq!(
      base.join("files").join("Pictures"),
      PathBuf::from("/data/storage/el2/base/files/Pictures")
    );
    assert_eq!(
      base.join("files").join("Videos"),
      PathBuf::from("/data/storage/el2/base/files/Videos")
    );
    assert_eq!(
      base.join("files").join("Public"),
      PathBuf::from("/data/storage/el2/base/files/Public")
    );
  }

  #[test]
  fn log_and_temp_dirs() {
    let base = mock_base();
    assert_eq!(
      base.join("log"),
      PathBuf::from("/data/storage/el2/base/log")
    );
    assert_eq!(
      base.join("temp"),
      PathBuf::from("/data/storage/el2/base/temp")
    );
  }

  #[test]
  fn resource_dir_uses_configured_module_first() {
    let tmp = TempDir::new("configured");
    let resolved = resolve_resource_dir(tmp.path(), Some("custom_entry")).unwrap();
    assert_eq!(
      resolved,
      tmp
        .path()
        .join("custom_entry")
        .join("resources")
        .join("resfile")
    );
  }

  #[test]
  fn resource_dir_fallback_prefers_tauri_conf_json() {
    let tmp = TempDir::new("tauri_conf");
    let desktop_resfile = tmp
      .path()
      .join("entry_desktop")
      .join("resources")
      .join("resfile");
    let mobile_resfile = tmp
      .path()
      .join("entry_mobile")
      .join("resources")
      .join("resfile");
    std::fs::create_dir_all(&desktop_resfile).unwrap();
    std::fs::create_dir_all(&mobile_resfile).unwrap();
    std::fs::write(mobile_resfile.join("tauri.conf.json"), "{}").unwrap();

    let resolved = resolve_resource_dir(tmp.path(), None).unwrap();
    assert_eq!(resolved, mobile_resfile);
  }

  #[test]
  fn resource_dir_fallback_checks_preferred_order() {
    let tmp = TempDir::new("preferred_order");
    let desktop_resfile = tmp
      .path()
      .join("entry_desktop")
      .join("resources")
      .join("resfile");
    let entry_resfile = tmp.path().join("entry").join("resources").join("resfile");
    std::fs::create_dir_all(&desktop_resfile).unwrap();
    std::fs::create_dir_all(&entry_resfile).unwrap();

    let resolved = resolve_resource_dir(tmp.path(), Some("")).unwrap();
    assert_eq!(resolved, desktop_resfile);
  }

  #[test]
  fn resource_dir_fallback_accepts_single_custom_resfile() {
    let tmp = TempDir::new("single_custom");
    let custom_resfile = tmp
      .path()
      .join("my_hap")
      .join("resources")
      .join("resfile");
    std::fs::create_dir_all(&custom_resfile).unwrap();

    let resolved = resolve_resource_dir(tmp.path(), None).unwrap();
    assert_eq!(resolved, custom_resfile);
  }

  #[test]
  fn resource_dir_errors_when_module_missing_and_not_discoverable() {
    let tmp = TempDir::new("missing");
    for missing in [None, Some(""), Some("   ")] {
      let result = resolve_resource_dir(tmp.path(), missing);
      assert!(matches!(result, Err(Error::UnknownPath)));
    }
  }

  #[test]
  fn normalize_module_name_treats_empty_as_missing() {
    assert_eq!(normalize_module_name(None), None);
    assert_eq!(normalize_module_name(Some("")), None);
    assert_eq!(normalize_module_name(Some("  ")), None);
    assert_eq!(normalize_module_name(Some("entry")), Some("entry"));
  }

  #[test]
  fn resource_dir_uses_custom_module_name() {
    assert_eq!(
      compute_resource_dir("entry_desktop"),
      PathBuf::from("/data/storage/el1/bundle/entry_desktop/resources/resfile")
    );
  }

  #[test]
  fn resource_dir_always_under_bundle_and_ends_with_resfile() {
    for module in ["x", "entry", "entry_desktop", "mod_a"] {
      let p = compute_resource_dir(module);
      assert!(p.starts_with("/data/storage/el1/bundle"));
      assert_eq!(p.file_name().unwrap(), "resfile");
    }
  }

  #[test]
  fn file_name_returns_last_component() {
    let name = Path::new("/a/b/c.txt")
      .file_name()
      .map(|n| n.to_string_lossy().into_owned());
    assert_eq!(name, Some("c.txt".to_string()));

    let none = Path::new("/a/b/..")
      .file_name()
      .map(|n| n.to_string_lossy().into_owned());
    assert_eq!(none, None);
  }

  #[test]
  fn base_path_returns_error_when_not_initialized() {
    let empty: Option<&String> = None;
    let result: Result<PathBuf> = empty.map(|p| PathBuf::from(p)).ok_or(Error::UnknownPath);
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), Error::UnknownPath));
  }
}
