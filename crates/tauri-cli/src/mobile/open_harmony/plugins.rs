// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crate::error::{bail, Context as ErrorContext};
use crate::Result;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct DetectedPlugin {
  pub name: String,
  pub har_path: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct OhPackage {
  pub name: String,
  pub version: String,
}

#[derive(Debug, Clone)]
pub struct PluginMeta {
  pub name: String,
  pub identifier: String,
  pub class_name: String,
  pub har_path: PathBuf,
}

pub fn detect_plugins(cargo_manifest_path: &Path) -> Result<Vec<String>> {
  let content = fs::read_to_string(cargo_manifest_path)
    .with_context(|| format!("failed to read {}", cargo_manifest_path.display()))?;

  let manifest: toml_edit::DocumentMut = content
    .parse::<toml_edit::DocumentMut>()
    .context("failed to parse Cargo.toml")?;

  let mut plugins = Vec::new();

  if let Some(deps) = manifest.get("dependencies").and_then(|d| d.as_table()) {
    collect_plugins_from_table(deps, &mut plugins);
  }

  if let Some(target_deps) = manifest.get("target").and_then(|t| t.as_table()) {
    for (_, target_val) in target_deps.iter() {
      if let Some(target_table) = target_val.as_table() {
        if let Some(deps) = target_table.get("dependencies").and_then(|d| d.as_table()) {
          collect_plugins_from_table(deps, &mut plugins);
        }
      }
    }
  }

  plugins.sort();
  plugins.dedup();

  log::info!("Detected {} plugins: {:?}", plugins.len(), plugins);

  Ok(plugins)
}

fn collect_plugins_from_table(table: &toml_edit::Table, plugins: &mut Vec<String>) {
  for (name, _) in table.iter() {
    if name.starts_with("tauri-plugin-") {
      plugins.push(name.replace("tauri-plugin-", ""));
    }
  }
}

pub fn find_plugin_har(plugin_name: &str, project_dir: &Path) -> Result<PathBuf> {
  validate_plugin_name(plugin_name)?;

  let canonical_project = project_dir
    .canonicalize()
    .context("failed to canonicalize project directory")?;

  // Build the candidate search paths. Order matters: the first existing path
  // wins, so more-specific (in-tree) candidates come first.
  let mut search_paths: Vec<PathBuf> = Vec::new();

  // 1. App in-tree plugins: `<app>/plugins/<name>/openharmony`.
  search_paths.push(
    canonical_project
      .join("plugins")
      .join(plugin_name)
      .join("openharmony"),
  );

  // 2. Walk up from the app's src-tauri dir to find a `plugins-workspace`
  //    sibling (or the app living inside one). Covers both the monorepo
  //    sibling layout (`<root>/<app>/src-tauri`) and the demo layout
  //    (`plugins-workspace/examples/<app>/src-tauri`, arbitrary depth) without
  //    hard-coding a fixed number of `parent()` hops.
  if let Some(root) = find_ancestor_with_plugins_workspace(&canonical_project) {
    search_paths.push(
      root
        .join("plugins-workspace")
        .join("plugins")
        .join(plugin_name)
        .join("openharmony"),
    );
  }

  // 3. Workspace root resolved from CARGO_MANIFEST_DIR (source dev run) or
  //    TAURI_WORKSPACE_ROOT env (installed binary). Same ancestor walk as #2
  //    but anchored at the cli crate dir, so the two are independent: when
  //    running from source, #2 (anchored at the app) usually wins; for an
  //    installed binary whose CARGO_MANIFEST_DIR points at the build machine,
  //    only the env override yields a real path.
  let workspace_root = get_tauri_workspace_root();
  if !workspace_root.as_os_str().is_empty() {
    search_paths.push(
      workspace_root
        .join("plugins-workspace")
        .join("plugins")
        .join(plugin_name)
        .join("openharmony"),
    );
  }

  for path in &search_paths {
    if path.exists() {
      log::info!("Found plugin '{}' at: {}", plugin_name, path.display());
      return Ok(path.clone());
    }
  }

  log::warn!(
    "Plugin '{}' not found in any search path, may not support OpenHarmony",
    plugin_name
  );

  bail!(
    "Plugin '{}' OpenHarmony HAR not found. Searched paths:\n{}",
    plugin_name,
    search_paths
      .iter()
      .map(|p| p.display().to_string())
      .collect::<Vec<_>>()
      .join("\n")
  )
}

/// Walk up from `start` to the nearest ancestor directory that contains a
/// `plugins-workspace` child (or is itself named `plugins-workspace`), and
/// return that ancestor's parent (the monorepo root). Returns `None` if no
/// such ancestor exists. This makes plugin HAR discovery robust to the app
/// sitting at arbitrary depth inside a monorepo, instead of assuming a fixed
/// `parent().parent()` depth.
fn find_ancestor_with_plugins_workspace(start: &Path) -> Option<PathBuf> {
  let mut current = start;
  loop {
    if current.join("plugins-workspace").is_dir() {
      return Some(current.to_path_buf());
    }
    match current.parent() {
      Some(parent) => current = parent,
      None => return None,
    }
  }
}

pub fn detect_all_plugins(project_dir: &Path) -> Result<Vec<DetectedPlugin>> {
  let cargo_manifest = project_dir.join("Cargo.toml");

  if !cargo_manifest.exists() {
    bail!("Cargo.toml not found at {}", cargo_manifest.display())
  }

  let plugin_names = detect_plugins(&cargo_manifest)?;

  let mut detected: Vec<DetectedPlugin> = Vec::new();

  for name in &plugin_names {
    match find_plugin_har(name, project_dir) {
      Ok(har_path) => {
        detected.push(DetectedPlugin {
          name: name.clone(),
          har_path,
        });
      }
      Err(e) => {
        log::warn!("Skipping plugin '{}': {}", name, e);
      }
    }
  }

  log::info!("Successfully located {} plugin HARs", detected.len());

  Ok(detected)
}

fn get_tauri_workspace_root() -> PathBuf {
  // Explicit override wins: used by an installed tauri-cli binary whose
  // CARGO_MANIFEST_DIR points at the build machine and thus cannot locate the
  // workspace by walking ancestors.
  if let Ok(root) = std::env::var("TAURI_WORKSPACE_ROOT") {
    return PathBuf::from(root);
  }

  // Running from source: CARGO_MANIFEST_DIR points at the dev machine's
  // `tauri/crates/tauri-cli`. Walk up to the nearest ancestor containing a
  // `plugins-workspace` sibling (the monorepo root).
  let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
    .map(PathBuf::from)
    .unwrap_or_default();

  find_ancestor_with_plugins_workspace(&manifest_dir).unwrap_or_default()
}

pub fn parse_oh_package(har_path: &Path) -> Result<OhPackage> {
  let oh_package_path = har_path.join("oh-package.json5");

  if !oh_package_path.exists() {
    bail!(
      "oh-package.json5 not found at {}",
      oh_package_path.display()
    )
  }

  let content = fs::read_to_string(&oh_package_path)
    .with_context(|| format!("failed to read {}", oh_package_path.display()))?;

  let oh_package: OhPackage = parse_json5(&content)
    .with_context(|| format!("failed to parse {}", oh_package_path.display()))?;

  log::info!(
    "Parsed oh-package: name={}, version={}",
    oh_package.name,
    oh_package.version
  );

  Ok(oh_package)
}

pub fn infer_class_name(plugin_name: &str) -> String {
  let pascal = plugin_name
    .split('-')
    .map(|part| {
      let mut chars = part.chars();
      match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
      }
    })
    .collect::<String>();

  format!("{}Plugin", pascal)
}

pub fn parse_plugin_meta(har_path: &Path, plugin_name: &str) -> Result<PluginMeta> {
  let oh_package = parse_oh_package(har_path)?;

  let identifier = oh_package.name;

  let class_name =
    try_parse_class_name_from_index(har_path).unwrap_or_else(|| infer_class_name(plugin_name));

  Ok(PluginMeta {
    name: plugin_name.to_string(),
    identifier,
    class_name,
    har_path: har_path.to_path_buf(),
  })
}

fn try_parse_class_name_from_index(har_path: &Path) -> Option<String> {
  let index_path = har_path.join("src/main/ets/index.ets");

  if !index_path.exists() {
    return None;
  }

  let content = match fs::read_to_string(&index_path) {
    Ok(c) => c,
    Err(e) => {
      log::warn!("Failed to read index.ets: {}", e);
      return None;
    }
  };

  // Patterns cover the export forms plugins use to surface their ArkTS class:
  //  - `export { default as <Class>Plugin }`   (default-as-Class re-export)
  //  - `export { <Class>Plugin as default }`   (Class-as-default, used by the
  //    dialog/notification/global-shortcut built-ins after relocation)
  //  - `export default class <Class>Plugin`
  //  - `export class <Class>Plugin extends Plugin`
  let patterns = [
    r"export\s+\{\s*\w+\s+as\s+(\w+Plugin)\s*\}",
    r"export\s+\{\s*(\w+Plugin)\s+as\s+\w+\s*\}",
    r"export\s+default\s+class\s+(\w+Plugin)",
    r"export\s+class\s+(\w+Plugin)\s+extends\s+Plugin",
  ];

  for pattern in &patterns {
    let re = regex::Regex::new(pattern).ok()?;
    for caps in re.captures_iter(&content) {
      if let Some(m) = caps.get(1) {
        let class_name = m.as_str();
        if validate_class_name_pattern(class_name) {
          return Some(class_name.to_string());
        }
      }
    }
  }

  None
}

fn validate_class_name_pattern(name: &str) -> bool {
  name.ends_with("Plugin")
    && name.len() > 6
    && name.chars().all(|c| c.is_ascii_alphabetic())
    && name
      .chars()
      .next()
      .map(|c| c.is_uppercase())
      .unwrap_or(false)
}

fn parse_json5<T: DeserializeOwned>(content: &str) -> Result<T> {
  json5::from_str(content).context("failed to parse JSON5 content")
}

pub fn validate_plugin_name(name: &str) -> Result<()> {
  if name.is_empty() || name.len() > 64 {
    bail!("Plugin name must be 1-64 characters")
  }
  if !name
    .chars()
    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
  {
    bail!("Plugin name must only contain alphanumeric, hyphen, or underscore")
  }
  if name.starts_with('-') || name.starts_with('_') {
    bail!("Plugin name cannot start with hyphen or underscore")
  }
  Ok(())
}

/// Validates an ohpm package name - the `name` field of a plugin HAR's
/// oh-package.json5. Accepts both plain (`plugin-dialog`) and scoped
/// (`@tauri/plugin-dialog`) names: each part may only contain ASCII letters,
/// digits, `-`, `_` and `.`, and must neither be empty nor start/end with `.`
/// or `_` (the directory and import path constraints ohpm itself enforces).
///
/// Deliberately not restricted to the official `@tauri/plugin-` prefix: the
/// app-local plugin layout (`<src-tauri>/plugins/<name>/openharmony`, see
/// [`find_plugin_har`]) ships HARs under their own package names.
fn validate_identifier(identifier: &str) -> Result<()> {
  let parts: Vec<&str> = if let Some(scoped) = identifier.strip_prefix('@') {
    let Some((scope, name)) = scoped.split_once('/') else {
      bail!("Scoped ohpm package name '{identifier}' is missing a '/' after the scope");
    };
    vec![scope, name]
  } else {
    vec![identifier]
  };

  for part in parts {
    if part.is_empty() {
      bail!("ohpm package name '{identifier}' has an empty part");
    }
    if !part
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
      bail!("ohpm package name '{identifier}' may only contain letters, digits, '-', '_' and '.'");
    }
    if part.starts_with(['.', '_']) || part.ends_with(['.', '_']) {
      bail!("ohpm package name '{identifier}' has a part starting or ending with '.' or '_'");
    }
  }

  Ok(())
}

fn validate_class_name(class_name: &str) -> Result<()> {
  if !class_name.ends_with("Plugin") {
    bail!("Class name must end with 'Plugin'")
  }
  let base = class_name.trim_end_matches("Plugin");
  if base.is_empty() {
    bail!("Class name base cannot be empty")
  }
  if !base.chars().all(|c| c.is_ascii_alphabetic()) {
    bail!("Class name base must only contain alphabetic characters")
  }
  if !base
    .chars()
    .next()
    .map(|c| c.is_uppercase())
    .unwrap_or(false)
  {
    bail!("Class name must start with uppercase letter")
  }
  Ok(())
}

pub fn validate_plugin_meta(meta: &PluginMeta) -> Result<()> {
  validate_plugin_name(&meta.name)?;
  validate_identifier(&meta.identifier)?;
  validate_class_name(&meta.class_name)?;
  Ok(())
}

pub fn copy_plugin_har(meta: &PluginMeta, dest_dir: &Path) -> Result<PathBuf> {
  validate_plugin_name(&meta.name)?;

  let canonical_dest = dest_dir
    .canonicalize()
    .context("failed to canonicalize destination directory")?;

  let canonical_har = meta
    .har_path
    .canonicalize()
    .context("failed to canonicalize plugin HAR path")?;

  let plugin_dest = canonical_dest.join(&meta.name);

  log::info!(
    "Copying plugin '{}' from {} to {}",
    meta.name,
    canonical_har.display(),
    plugin_dest.display()
  );

  if plugin_dest.exists() {
    fs::remove_dir_all(&plugin_dest).with_context(|| {
      format!(
        "failed to remove existing plugin directory at {}",
        plugin_dest.display()
      )
    })?;
  }

  fs::create_dir_all(&plugin_dest).with_context(|| {
    format!(
      "failed to create plugin directory at {}",
      plugin_dest.display()
    )
  })?;

  for entry in WalkDir::new(&canonical_har)
    .into_iter()
    .filter_map(|e| e.ok())
  {
    let src_path = entry.path();
    let relative = src_path
      .strip_prefix(&canonical_har)
      .context("failed to strip prefix from source path")?;

    // Skip build artifacts: `.tauri/` is the generated `@tauri/app` runtime HAR
    // (produced by `tauri_plugin::Builder::ohos_path`), `target/` is Rust build
    // output. Copying either into the generated project would duplicate the
    // runtime already provided by the `tauri/` module and pollute `oh-package`.
    if relative.starts_with(".tauri") || relative.starts_with("target") {
      continue;
    }

    verify_relative_path_safe(relative)?;

    let dest_path = plugin_dest.join(relative);

    if entry.file_type().is_dir() {
      fs::create_dir_all(&dest_path)
        .with_context(|| format!("failed to create directory {}", dest_path.display()))?;
    } else {
      verify_path_within_destination(&dest_path, &plugin_dest)?;
      fs::copy(src_path, &dest_path).with_context(|| {
        format!(
          "failed to copy {} to {}",
          src_path.display(),
          dest_path.display()
        )
      })?;

      if relative.ends_with("oh-package.json5") || relative.ends_with("build-profile.json5") {
        adjust_paths_in_file(&dest_path)?;
      }
    }
  }

  log::info!("Successfully copied plugin '{}' HAR", meta.name);
  Ok(plugin_dest)
}

fn verify_relative_path_safe(relative: &Path) -> Result<()> {
  for component in relative.components() {
    match component {
      std::path::Component::Prefix(_) | std::path::Component::RootDir => {
        bail!("Absolute path component in relative path")
      }
      std::path::Component::ParentDir => {
        bail!("Parent directory traversal in path")
      }
      std::path::Component::Normal(name) => {
        let name_str = name.to_string_lossy();
        if name_str.contains("..") || name_str.contains('\\') {
          bail!("Unsafe path component: {}", name_str)
        }
      }
      std::path::Component::CurDir => {}
    }
  }
  Ok(())
}

fn verify_path_within_destination(dest_path: &Path, dest_root: &Path) -> Result<()> {
  let dest_path_str = dest_path.to_string_lossy();
  let dest_root_str = dest_root.to_string_lossy();

  if !dest_path_str.starts_with(&*dest_root_str) {
    bail!(
      "Destination path '{}' is outside plugin directory '{}'",
      dest_path.display(),
      dest_root.display()
    )
  }

  Ok(())
}

fn adjust_paths_in_file(file_path: &Path) -> Result<()> {
  let content = fs::read_to_string(file_path)
    .with_context(|| format!("failed to read {}", file_path.display()))?;

  let adjusted = content
    .replace(
      "\"@tauri/app\": \"file:../../../tauri\"",
      "\"@tauri/app\": \"file:../tauri\"",
    )
    .replace(
      "\"@tauri/app\": \"file:../../tauri\"",
      "\"@tauri/app\": \"file:../tauri\"",
    )
    .replace("../../../tauri", "../tauri")
    .replace("../../tauri", "../tauri");

  fs::write(file_path, adjusted)
    .with_context(|| format!("failed to write {}", file_path.display()))?;
  Ok(())
}

pub fn update_build_profile(project_dir: &Path, plugins: &[PluginMeta]) -> Result<()> {
  let build_profile_path = project_dir.join("build-profile.json5");

  log::info!(
    "Updating build-profile.json5 with {} plugins",
    plugins.len()
  );

  let content = fs::read_to_string(&build_profile_path)
    .with_context(|| "failed to read build-profile.json5")?;

  let mut profile: Value = parse_json5(&content).context("failed to parse build-profile.json5")?;

  let modules = profile
    .get_mut("modules")
    .and_then(|v| v.as_array_mut())
    .with_context(|| "build-profile.json5 has no 'modules' array")?;

  for plugin in plugins {
    // OHOS module names must match ^[a-zA-Z][0-9a-zA-Z_.]*$ (no hyphens)
    let module_name = plugin.name.replace('-', "");
    if !modules
      .iter()
      .any(|m| m.get("name").and_then(|v| v.as_str()) == Some(&module_name))
    {
      log::info!("Adding module for plugin '{}'", plugin.name);

      let module = serde_json::json!({
        "name": module_name,
        "srcPath": format!("./{}", plugin.name),
        "targets": [{
          "name": "default",
          "applyToProducts": ["default"]
        }]
      });

      modules.push(module);
    } else {
      log::info!("Module '{}' already exists, skipping", plugin.name);
    }
  }

  let updated = serialize_json5(&profile)?;
  fs::write(&build_profile_path, updated).with_context(|| "failed to write build-profile.json5")?;

  log::info!("Successfully updated build-profile.json5");
  Ok(())
}

/// Rewrite `build-profile.json5`'s `modules` array to activate exactly the
/// given entry modules - `["entry_mobile"]` for a single-form build,
/// `["entry_mobile", "entry_desktop"]` for `--app` - preserving the shared
/// non-entry modules already present (`tauri`, `dialog`, ...). This selects
/// which entry HAP(s) hvigor builds.
///
/// Note: entry modules are rebuilt from scratch (only `name`/`srcPath`/`targets`
/// are written). User customizations on an entry's build-profile module object
/// (e.g. an added `buildOption`) would be dropped - the template-generated
/// entries don't carry any, so this is acceptable. Non-entry modules are
/// preserved verbatim.
pub fn write_build_profile_modules(project_dir: &Path, active_entries: &[&str]) -> Result<()> {
  let build_profile_path = project_dir.join("build-profile.json5");
  let content = fs::read_to_string(&build_profile_path)
    .with_context(|| "failed to read build-profile.json5")?;
  let mut profile: Value = parse_json5(&content).context("failed to parse build-profile.json5")?;

  // Keep non-entry modules (tauri, dialog, ...); drop any entry-* module so we
  // can re-insert only the active ones.
  let kept: Vec<Value> = {
    let modules = profile
      .get_mut("modules")
      .and_then(|m| m.as_array_mut())
      .with_context(|| "build-profile.json5 has no 'modules' array")?;
    let mut kept = Vec::new();
    for m in modules.iter() {
      let is_entry = m
        .get("name")
        .and_then(|n| n.as_str())
        .map(|n| n.starts_with("entry"))
        .unwrap_or(false);
      if !is_entry {
        kept.push(m.clone());
      }
    }
    kept
  };

  let mut new_modules: Vec<Value> = active_entries
    .iter()
    .map(|name| {
      serde_json::json!({
        "name": name,
        "srcPath": format!("./{name}"),
        "targets": [{ "name": "default", "applyToProducts": ["default"] }]
      })
    })
    .collect();
  new_modules.extend(kept);

  {
    let modules = profile
      .get_mut("modules")
      .and_then(|m| m.as_array_mut())
      .with_context(|| "build-profile.json5 has no 'modules' array")?;
    *modules = new_modules;
  }

  let updated = serialize_json5(&profile)?;
  fs::write(&build_profile_path, updated).with_context(|| "failed to write build-profile.json5")?;
  log::info!("Activated entry modules: {:?}", active_entries);
  Ok(())
}

/// Rewrite `entry_{form}/src/main/module.json5`'s `deviceTypes` to the given
/// subset, so conf `deviceTypes` changes take effect on rebuild without
/// re-running `ohos init`. `form` selects the entry module (`entry_{form}`).
pub fn write_entry_device_types(
  project_dir: &Path,
  form: &str,
  device_types: &[String],
) -> Result<()> {
  let module = format!("entry_{form}");
  let module_json = project_dir.join(format!("{module}/src/main/module.json5"));
  let content = fs::read_to_string(&module_json)
    .with_context(|| format!("failed to read {module}/src/main/module.json5"))?;
  let mut doc: Value =
    parse_json5(&content).with_context(|| format!("failed to parse {module} module.json5"))?;
  let module_obj = doc
    .get_mut("module")
    .and_then(|m| m.as_object_mut())
    .with_context(|| format!("{module}/src/main/module.json5 has no 'module' object"))?;
  module_obj.insert(
    "deviceTypes".to_string(),
    Value::Array(
      device_types
        .iter()
        .map(|d| Value::String(d.clone()))
        .collect(),
    ),
  );
  let updated = serialize_json5(&doc)?;
  fs::write(&module_json, updated)
    .with_context(|| format!("failed to write {module}/src/main/module.json5"))?;
  Ok(())
}

/// Rewrite `entry_{form}/src/main/module.json5`'s `continuable`/`continueType`
/// (app continuation gating) per the conf's `bundle.openHarmony` settings, so
/// conf changes take effect on rebuild without re-running `ohos init` (same
/// injection point as [`write_entry_device_types`]).
///
/// `continuable == Some(true)` writes `continuable: true` plus `continueType`
/// (`continue_type` when provided, else falling back to `[identifier]` - the
/// same app installed on both devices then matches automatically). Any other
/// `continuable` value **removes** both keys, so toggling back to disabled also
/// takes effect on rebuild.
pub fn write_entry_continuation(
  project_dir: &Path,
  form: &str,
  continuable: Option<bool>,
  continue_type: Option<&[String]>,
  identifier: &str,
) -> Result<()> {
  let module = format!("entry_{form}");
  let module_json = project_dir.join(format!("{module}/src/main/module.json5"));
  let content = fs::read_to_string(&module_json)
    .with_context(|| format!("failed to read {module}/src/main/module.json5"))?;
  let mut doc: Value =
    parse_json5(&content).with_context(|| format!("failed to parse {module} module.json5"))?;
  let ability = doc
    .get_mut("module")
    .and_then(|m| m.get_mut("abilities"))
    .and_then(|a| a.as_array_mut())
    .and_then(|a| a.get_mut(0))
    .and_then(|a| a.as_object_mut())
    .with_context(|| format!("{module}/src/main/module.json5 has no abilities[0] object"))?;

  if continuable == Some(true) {
    let types: Vec<String> = match continue_type {
      Some(types) if !types.is_empty() => types.to_vec(),
      // Same app on both devices matches automatically via the shared identifier.
      _ => vec![identifier.to_string()],
    };
    ability.insert("continuable".to_string(), Value::Bool(true));
    ability.insert(
      "continueType".to_string(),
      Value::Array(types.into_iter().map(Value::String).collect()),
    );
  } else {
    // Removing a missing key is a no-op - safe toggle-back to disabled.
    ability.remove("continuable");
    ability.remove("continueType");
  }

  let updated = serialize_json5(&doc)?;
  fs::write(&module_json, updated)
    .with_context(|| format!("failed to write {module}/src/main/module.json5"))?;
  Ok(())
}

pub fn update_entry_package(project_dir: &Path, plugins: &[PluginMeta]) -> Result<()> {
  let entry_module = super::active_entry_module();
  let bridge_specs = detect_required_bridge_plugins(project_dir);
  update_entry_package_for_module(project_dir, &entry_module, plugins, &bridge_specs)
}

fn update_entry_package_for_module(
  project_dir: &Path,
  entry_module: &str,
  plugins: &[PluginMeta],
  bridge_specs: &[&BridgePluginSpec],
) -> Result<()> {
  let entry_dir = project_dir.join(entry_module);
  let oh_package_path = entry_dir.join("oh-package.json5");

  log::info!(
    "Updating {entry_module}/oh-package.json5 with {} JS plugins and {} bridge plugins",
    plugins.len(),
    bridge_specs.len()
  );

  let content = fs::read_to_string(&oh_package_path)
    .with_context(|| format!("failed to read {entry_module}/oh-package.json5"))?;

  let mut package: Value =
    parse_json5(&content).context("failed to parse entry oh-package.json5")?;

  let dependencies = package
    .get_mut("dependencies")
    .and_then(|v| v.as_object_mut())
    .with_context(|| format!("{entry_module}/oh-package.json5 has no 'dependencies' object"))?;

  let ability_dir = super::project::find_sibling_ability_dir(project_dir);
  sync_bridge_plugin_dependencies(dependencies, ability_dir.as_deref(), &entry_dir, bridge_specs);

  for plugin in plugins {
    if !dependencies.contains_key(&plugin.identifier) {
      log::info!(
        "Adding dependency '{}' -> 'file:../{}'",
        plugin.identifier,
        plugin.name
      );

      dependencies.insert(
        plugin.identifier.clone(),
        Value::String(format!("file:../{}", plugin.name)),
      );
    } else {
      log::info!("Dependency '{}' already exists", plugin.identifier);
    }
  }

  let updated = serialize_json5(&package)?;
  fs::write(&oh_package_path, updated)
    .with_context(|| format!("failed to write {entry_module}/oh-package.json5"))?;

  log::info!("Successfully updated {entry_module}/oh-package.json5");

  update_entry_ability(project_dir, entry_module, plugins, bridge_specs)?;

  Ok(())
}

/// Idempotently syncs missing `@ohos-rs/ability` and modular
/// `@ohos-rs/ability-plugin-*` dependency entries into an entry module's
/// `oh-package.json5` `dependencies` map.
pub(crate) fn sync_bridge_plugin_dependencies(
  dependencies: &mut serde_json::Map<String, Value>,
  ability_dir: Option<&Path>,
  entry_dir: &Path,
  bridge_specs: &[&BridgePluginSpec],
) {
  if !dependencies.contains_key(super::project::ABILITY_PACKAGE_NAME) {
    let dep = super::project::resolve_ability_dependency(
      ability_dir,
      entry_dir,
      super::project::ABILITY_PACKAGE_NAME,
      super::project::ABILITY_DEFAULT_VERSION,
    );
    dependencies.insert(
      super::project::ABILITY_PACKAGE_NAME.to_string(),
      Value::String(dep),
    );
  }

  for spec in bridge_specs {
    if !dependencies.contains_key(spec.ohpm_package) {
      let dep = super::project::resolve_ability_dependency(
        ability_dir,
        entry_dir,
        spec.ohpm_package,
        spec.default_version,
      );
      log::info!(
        "Adding modular bridge dependency '{}' -> '{}'",
        spec.ohpm_package,
        dep
      );
      dependencies.insert(spec.ohpm_package.to_string(), Value::String(dep));
    }
  }
}

/// Idempotently sync the plugin imports and `STATIC_PLUGINS` registrations in
/// an entry module's `EntryAbility.ets` with the detected plugin set, and the
/// modular `@ohos-rs/ability-plugin-*` bridge plugin factories with `bridge_specs`.
///
/// Additive-only: existing lines (including hand edits) are never rewritten or
/// removed; entries for plugins that are no longer detected are left in place.
pub fn update_entry_ability(
  project_dir: &Path,
  entry_module: &str,
  plugins: &[PluginMeta],
  bridge_specs: &[&BridgePluginSpec],
) -> Result<()> {
  let ability_path = project_dir
    .join(entry_module)
    .join("src/main/ets/entryability/EntryAbility.ets");

  let content = fs::read_to_string(&ability_path)
    .with_context(|| format!("failed to read {}", ability_path.display()))?;

  let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

  // Anchors: the `@tauri/app` import for new plugin imports, and the
  // STATIC_PLUGINS declaration (or its last `.set(` line) for new
  // registrations. If the file was not generated from the standard template
  // (no import anchor), still try syncing bridge factories.
  if let Some(import_anchor) = lines
    .iter()
    .position(|l| l.trim_start().starts_with("import") && l.contains("from '@tauri/app'"))
  {
    let mut reg_anchor = lines
      .iter()
      .rposition(|l| l.contains("STATIC_PLUGINS.set("))
      .or_else(|| {
        lines
          .iter()
          .position(|l| l.contains("const STATIC_PLUGINS"))
      });

    for plugin in plugins {
      let import_line = format!("import {} from '{}';", plugin.class_name, plugin.identifier);
      let reg_line = format!(
        "STATIC_PLUGINS.set('{}', new {}());",
        plugin.name, plugin.class_name
      );

      if !lines
        .iter()
        .any(|l| l.contains(&format!("from '{}'", plugin.identifier)))
      {
        log::info!(
          "Adding EntryAbility import for plugin '{}': {}",
          plugin.name,
          import_line
        );
        lines.insert(import_anchor + 1, import_line);
        if let Some(anchor) = reg_anchor.as_mut() {
          *anchor += 1;
        }
      }

      let has_reg = lines
        .iter()
        .any(|l| l.contains(&format!("STATIC_PLUGINS.set('{}'", plugin.name)));
      if !has_reg {
        if let Some(anchor) = reg_anchor {
          log::info!(
            "Adding STATIC_PLUGINS entry for plugin '{}': {}",
            plugin.name,
            reg_line
          );
          lines.insert(anchor + 1, reg_line);
          if let Some(a) = reg_anchor.as_mut() {
            *a += 1;
          }
        } else {
          log::warn!(
            "{entry_module}/EntryAbility.ets has no STATIC_PLUGINS anchor; cannot register plugin '{}'",
            plugin.name
          );
        }
      }
    }
  } else if !plugins.is_empty() {
    log::warn!(
      "{entry_module}/EntryAbility.ets has no '@tauri/app' import anchor; skipping JS plugin registry sync"
    );
  }

  sync_bridge_plugin_factories(&mut lines, entry_module, bridge_specs);

  let mut updated = lines.join("\n");
  if !updated.ends_with('\n') {
    updated.push('\n');
  }
  fs::write(&ability_path, updated)
    .with_context(|| format!("failed to write {}", ability_path.display()))?;

  Ok(())
}

/// Canonical specification for a modular `@ohos-rs/ability-plugin-*` bridge
/// plugin shipped by `openharmony-ability`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BridgePluginSpec {
  /// Rust crate name (e.g. `openharmony-ability-plugin-window`).
  pub crate_name: &'static str,
  /// OHPM package name (e.g. `@ohos-rs/ability-plugin-window`).
  pub ohpm_package: &'static str,
  /// Exported ArkTS plugin class name (e.g. `WindowPlugin`).
  pub ets_class_name: &'static str,
  /// Optional local import alias to avoid collision with a Tauri JS plugin class.
  pub local_alias: Option<&'static str>,
  /// Default OHPM registry semver range when no local packed `.har` is present.
  pub default_version: &'static str,
  /// Whether this plugin is required by Tauri's core OHOS runtime and included
  /// by default in generated `EntryAbility.ets` and `oh-package.json5`.
  pub core: bool,
  /// Optional matching `tauri-plugin-*` Cargo crate name that pulls this bridge
  /// plugin in transitively.
  pub tauri_plugin_crate: Option<&'static str>,
}

impl BridgePluginSpec {
  pub(crate) fn local_name(&self) -> &'static str {
    self.local_alias.unwrap_or(self.ets_class_name)
  }

  pub(crate) fn import_statement(&self) -> String {
    match self.local_alias {
      Some(alias) => format!(
        "import {{ {} as {} }} from '{}';",
        self.ets_class_name, alias, self.ohpm_package
      ),
      None => format!(
        "import {{ {} }} from '{}';",
        self.ets_class_name, self.ohpm_package
      ),
    }
  }

  pub(crate) fn template_dep_key(&self) -> String {
    let suffix = self
      .ohpm_package
      .strip_prefix("@ohos-rs/")
      .unwrap_or(self.ohpm_package);
    format!("{suffix}-dep")
  }
}

/// Canonical 21-plugin table matching `openharmony-ability@63be763`.
pub(crate) const BRIDGE_PLUGINS: &[BridgePluginSpec] = &[
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-accessibility",
    ohpm_package: "@ohos-rs/ability-plugin-accessibility",
    ets_class_name: "AccessibilityPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-accessibility"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-account",
    ohpm_package: "@ohos-rs/ability-plugin-account",
    ets_class_name: "AccountPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-account"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-app-control",
    ohpm_package: "@ohos-rs/ability-plugin-app-control",
    ets_class_name: "AppControlPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: true,
    tauri_plugin_crate: None,
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-audio",
    ohpm_package: "@ohos-rs/ability-plugin-audio",
    ets_class_name: "AudioPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.4",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-audio"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-autostart",
    ohpm_package: "@ohos-rs/ability-plugin-autostart",
    ets_class_name: "AutostartPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-autostart"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-clipboard",
    ohpm_package: "@ohos-rs/ability-plugin-clipboard",
    ets_class_name: "ClipboardPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-clipboard-manager"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-deep-link",
    ohpm_package: "@ohos-rs/ability-plugin-deep-link",
    ets_class_name: "DeepLinkPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-deep-link"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-fault-injection",
    ohpm_package: "@ohos-rs/ability-plugin-fault-injection",
    ets_class_name: "FaultInjectionPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: None,
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-files",
    ohpm_package: "@ohos-rs/ability-plugin-files",
    ets_class_name: "FilesPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: true,
    tauri_plugin_crate: Some("tauri-plugin-fs"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-global-shortcut",
    ohpm_package: "@ohos-rs/ability-plugin-global-shortcut",
    ets_class_name: "GlobalShortcutPlugin",
    local_alias: Some("GlobalShortcutBridgePlugin"),
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-global-shortcut"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-menu",
    ohpm_package: "@ohos-rs/ability-plugin-menu",
    ets_class_name: "MenuPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: true,
    tauri_plugin_crate: None,
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-notification",
    ohpm_package: "@ohos-rs/ability-plugin-notification",
    ets_class_name: "NotificationPlugin",
    local_alias: Some("NotificationBridgePlugin"),
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-notification"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-permission",
    ohpm_package: "@ohos-rs/ability-plugin-permission",
    ets_class_name: "PermissionPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: true,
    tauri_plugin_crate: None,
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-process",
    ohpm_package: "@ohos-rs/ability-plugin-process",
    ets_class_name: "ProcessPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-process"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-resource",
    ohpm_package: "@ohos-rs/ability-plugin-resource",
    ets_class_name: "ResourcePlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: true,
    tauri_plugin_crate: None,
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-statusbar",
    ohpm_package: "@ohos-rs/ability-plugin-statusbar",
    ets_class_name: "StatusbarPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: true,
    tauri_plugin_crate: None,
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-system-state",
    ohpm_package: "@ohos-rs/ability-plugin-system-state",
    ets_class_name: "SystemStatePlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-os"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-updater",
    ohpm_package: "@ohos-rs/ability-plugin-updater",
    ets_class_name: "UpdaterPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: false,
    tauri_plugin_crate: Some("tauri-plugin-updater"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-url",
    ohpm_package: "@ohos-rs/ability-plugin-url",
    ets_class_name: "UrlPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: true,
    tauri_plugin_crate: Some("tauri-plugin-opener"),
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-webview",
    ohpm_package: "@ohos-rs/ability-plugin-webview",
    ets_class_name: "WebviewPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: true,
    tauri_plugin_crate: None,
  },
  BridgePluginSpec {
    crate_name: "openharmony-ability-plugin-window",
    ohpm_package: "@ohos-rs/ability-plugin-window",
    ets_class_name: "WindowPlugin",
    local_alias: None,
    default_version: "^1.0.0-beta.5",
    core: true,
    tauri_plugin_crate: None,
  },
];

/// Determines which modular `@ohos-rs/ability-plugin-*` packages should be
/// synced for a generated project: always includes the 9 core Tauri bridge
/// plugins plus any extra bridge plugins referenced in the app's `Cargo.toml`
/// or `Cargo.lock`.
fn detect_required_bridge_plugins(project_dir: &Path) -> Vec<&'static BridgePluginSpec> {
  // project_dir is typically `<src-tauri>/gen/ohos`.
  let tauri_dir = project_dir.parent().and_then(|p| p.parent());
  let mut manifest_corpus = String::new();
  if let Some(dir) = tauri_dir {
    if let Ok(cargo_toml) = fs::read_to_string(dir.join("Cargo.toml")) {
      manifest_corpus.push_str(&cargo_toml);
      manifest_corpus.push('\n');
    }
    if let Ok(cargo_lock) = fs::read_to_string(dir.join("Cargo.lock")) {
      manifest_corpus.push_str(&cargo_lock);
    }
  }
  select_bridge_plugins_for_manifest(&manifest_corpus)
}

fn select_bridge_plugins_for_manifest(manifest_corpus: &str) -> Vec<&'static BridgePluginSpec> {
  BRIDGE_PLUGINS
    .iter()
    .filter(|spec| {
      spec.core
        || manifest_corpus.contains(spec.crate_name)
        || spec
          .tauri_plugin_crate
          .is_some_and(|crate_name| manifest_corpus.contains(crate_name))
    })
    .collect()
}

/// Backfills modular per-package `@ohos-rs/ability-plugin-*` imports and
/// `new LazyPlugin(() => new XxxPlugin()),` entries inside `public bridgePlugins = [ ... ]`.
///
/// Additive-only: existing imports and factories (including hand edits) are
/// left untouched. Files predating the bridge plugin architecture (missing an
/// `@ohos-rs/ability` import or `bridgePlugins = [` array) are skipped with a
/// warning.
fn sync_bridge_plugin_factories(
  lines: &mut Vec<String>,
  entry_module: &str,
  bridge_specs: &[&BridgePluginSpec],
) {
  let Some(ability_import_idx) = lines.iter().position(|l| {
    l.contains("from '@ohos-rs/ability'") || l.contains("from \"@ohos-rs/ability\"")
  }) else {
    log::warn!(
      "{entry_module}/EntryAbility.ets has no '@ohos-rs/ability' import; it predates the bridge plugin architecture — re-run `tauri ohos init` to regenerate"
    );
    return;
  };

  let import_anchor = lines
    .iter()
    .rposition(|l| {
      l.contains("from '@ohos-rs/ability-plugin-")
        || l.contains("from \"@ohos-rs/ability-plugin-")
    })
    .unwrap_or(ability_import_idx);

  let Some(array_open) = lines.iter().position(|l| l.contains("bridgePlugins = [")) else {
    log::warn!(
      "{entry_module}/EntryAbility.ets has no `bridgePlugins` array; it predates the bridge plugin architecture — re-run `tauri ohos init` to regenerate"
    );
    return;
  };
  let Some(array_close) = (array_open + 1..lines.len()).find(|&i| {
    let trimmed = lines[i].trim();
    trimmed == "]" || trimmed == "];"
  }) else {
    log::warn!(
      "{entry_module}/EntryAbility.ets has a malformed `bridgePlugins` array; skipping bridge plugin sync"
    );
    return;
  };

  let mut missing: Vec<(&BridgePluginSpec, bool)> = Vec::new();
  for spec in bridge_specs {
    let local_name = spec.local_name();
    if lines
      .iter()
      .any(|l| l.contains(&format!("new {local_name}()")))
    {
      continue;
    }
    let has_import = lines.iter().any(|l| {
      l.contains(&format!("from '{}'", spec.ohpm_package))
        || l.contains(&format!("from \"{}\"", spec.ohpm_package))
        || l.contains(local_name)
    });
    missing.push((*spec, !has_import));
  }
  if missing.is_empty() {
    return;
  }

  // Insert factory lines (higher index) before import lines (lower index) so
  // neither insertion index shifts while the other is still in use.
  for (offset, (spec, _)) in missing.iter().enumerate() {
    let local_name = spec.local_name();
    log::info!("Adding EntryAbility bridgePlugins entry for '{local_name}'");
    lines.insert(
      array_close + offset,
      format!("    new LazyPlugin(() => new {local_name}()),"),
    );
  }

  let mut insert_at = import_anchor + 1;
  for (spec, needs_import) in &missing {
    if *needs_import {
      let stmt = spec.import_statement();
      log::info!("Adding EntryAbility modular bridge import: {stmt}");
      lines.insert(insert_at, stmt);
      insert_at += 1;
    }
  }
}

/// Serializes `value` into JSON5-ish text.
///
/// Lossy round-trip: this pretty-prints with serde_json and re-adds trailing
/// commas line by line - any comments in the original file are already gone
/// (the file was parsed into a `serde_json::Value` beforehand), and key order
/// follows serde_json's map ordering, not the source file's.
fn serialize_json5(value: &Value) -> Result<String> {
  let json = serde_json::to_string_pretty(value).context("failed to serialize JSON5")?;

  let lines: Vec<&str> = json.lines().collect();

  let formatted = lines
    .iter()
    .enumerate()
    .map(|(idx, line)| {
      let trimmed = line.trim();
      let next_trimmed = lines.get(idx + 1).map(|l| l.trim()).unwrap_or("");

      // Lines that already end with a separator, open a nested block, or are
      // immediately followed by a closing bracket must not gain a comma.
      if trimmed.is_empty()
        || trimmed.starts_with("//")
        || next_trimmed.starts_with("}")
        || next_trimmed.starts_with("]")
        || trimmed.ends_with('{')
        || trimmed.ends_with('[')
        || trimmed.ends_with(',')
        || trimmed.ends_with('}')
        || trimmed.ends_with(']')
      {
        line.to_string()
      } else {
        format!("{line},")
      }
    })
    .collect::<Vec<_>>()
    .join("\n");

  Ok(formatted)
}

pub fn verify_plugin_before_update(plugin: &PluginMeta, project_dir: &Path) -> Result<()> {
  let plugin_dir = project_dir.join(&plugin.name);

  if !plugin_dir.exists() {
    bail!(
      "Plugin HAR directory '{}' does not exist at {}",
      plugin.name,
      plugin_dir.display()
    )
  }

  let oh_package_path = plugin_dir.join("oh-package.json5");
  if !oh_package_path.exists() {
    bail!(
      "Required oh-package.json5 missing in plugin '{}'",
      plugin.name
    )
  }

  let oh_package = parse_oh_package(&plugin_dir)?;
  if oh_package.name != plugin.identifier {
    bail!(
      "Plugin identifier mismatch: expected '{}', found '{}' in oh-package.json5",
      plugin.identifier,
      oh_package.name
    )
  }

  Ok(())
}

pub fn verify_all_plugins_before_update(plugins: &[PluginMeta], project_dir: &Path) -> Result<()> {
  for plugin in plugins {
    verify_plugin_before_update(plugin, project_dir)
      .with_context(|| format!("Plugin '{}' verification failed", plugin.name))?;
  }
  Ok(())
}

pub fn update_plugin_configs(project_dir: &Path, plugins: &[PluginMeta]) -> Result<()> {
  log::info!("Updating configurations for {} plugins", plugins.len());

  verify_all_plugins_before_update(plugins, project_dir)?;

  update_build_profile(project_dir, plugins)?;

  update_entry_package(project_dir, plugins)?;

  log::info!("All plugin configurations updated successfully");
  Ok(())
}

pub fn validate_plugin_configs(project_dir: &Path, plugins: &[PluginMeta]) -> Result<()> {
  let build_profile_path = project_dir.join("build-profile.json5");
  let content = fs::read_to_string(&build_profile_path)
    .with_context(|| "failed to read build-profile.json5 for validation")?;
  for plugin in plugins {
    let module_name = plugin.name.replace('-', "");
    if !content.contains(&format!("\"name\": \"{}\"", module_name)) {
      bail!(
        "Plugin '{}' (module '{}') not found in build-profile.json5 modules",
        plugin.name,
        module_name
      )
    }
  }

  let oh_package_path =
    project_dir.join(format!("{}/oh-package.json5", super::active_entry_module()));
  let content = fs::read_to_string(&oh_package_path)
    .with_context(|| "failed to read entry oh-package.json5 for validation")?;
  for plugin in plugins {
    if !content.contains(&plugin.identifier) {
      bail!(
        "Plugin '{}' not found in entry oh-package.json5 dependencies",
        plugin.identifier
      )
    }
  }

  Ok(())
}

/// Full plugin injection pipeline shared by `ohos build` and `ohos dev`:
/// detect the app's Tauri plugins, parse and validate their HAR metadata,
/// copy each HAR into the generated project, and wire the build-profile /
/// oh-package / EntryAbility registrations.
///
/// Returns the plugin metadata so callers can re-run the per-rebuild steps
/// (`copy_plugin_har` + `update_plugin_configs`) in dev's watch loop.
pub fn inject_plugins(tauri_dir: &Path, project_dir: &Path) -> Result<Vec<PluginMeta>> {
  log::info!("Starting OpenHarmony dynamic plugin injection");

  let detected_plugins = detect_all_plugins(tauri_dir).context("Plugin detection failed")?;

  if detected_plugins.is_empty() {
    log::info!("No OpenHarmony JS/HAR plugins detected; syncing modular bridge plugins only");
    let entry_module = super::active_entry_module();
    if project_dir.join(format!("{entry_module}/oh-package.json5")).is_file() {
      update_entry_package(project_dir, &[])?;
    }
    return Ok(vec![]);
  }

  log::info!(
    "Detected {} OpenHarmony plugins: {:?}",
    detected_plugins.len(),
    detected_plugins.iter().map(|p| &p.name).collect::<Vec<_>>()
  );

  let metadata: Vec<PluginMeta> = detected_plugins
    .iter()
    .map(|d| parse_plugin_meta(&d.har_path, &d.name))
    .collect::<Result<Vec<_>>>()
    .context("Plugin metadata parsing failed")?;

  for plugin in &metadata {
    validate_plugin_meta(plugin)
      .context(format!("Invalid metadata for plugin '{}'", plugin.name))?;
  }

  for plugin in &metadata {
    copy_plugin_har(plugin, project_dir)
      .context(format!("Failed to copy plugin '{}' HAR", plugin.name))?;
  }

  update_plugin_configs(project_dir, &metadata)
    .context("Failed to update plugin configurations")?;

  validate_plugin_configs(project_dir, &metadata)
    .context("Plugin configuration validation failed")?;

  log::info!("Plugin injection completed with {} plugins", metadata.len());
  Ok(metadata)
}

/// Syncs modular bridge plugin dependencies and `EntryAbility.ets` factories
/// across both `entry_mobile` and `entry_desktop` after `ohos init`.
pub fn sync_bridge_plugins_for_all_entries(project_dir: &Path) -> Result<()> {
  let bridge_specs = detect_required_bridge_plugins(project_dir);
  for entry_module in ["entry_mobile", "entry_desktop"] {
    if project_dir.join(format!("{entry_module}/oh-package.json5")).is_file() {
      update_entry_package_for_module(project_dir, entry_module, &[], &bridge_specs)?;
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn canonical_bridge_plugin_table_has_all_21_plugins() {
    assert_eq!(BRIDGE_PLUGINS.len(), 21);
    let mut packages: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut crates: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for spec in BRIDGE_PLUGINS {
      assert!(packages.insert(spec.ohpm_package), "duplicate ohpm package {}", spec.ohpm_package);
      assert!(crates.insert(spec.crate_name), "duplicate crate {}", spec.crate_name);
      assert!(spec.ohpm_package.starts_with("@ohos-rs/ability-plugin-"));
      assert!(spec.crate_name.starts_with("openharmony-ability-plugin-"));
      assert!(spec.ets_class_name.ends_with("Plugin"));
    }
    // Verify stale non-existent plugins are NOT present.
    assert!(!BRIDGE_PLUGINS.iter().any(|s| s.ets_class_name == "WebviewCookiePlugin"));
    assert!(!BRIDGE_PLUGINS.iter().any(|s| s.ets_class_name == "WebviewUrlPlugin"));
    // Verify audio uses beta.4 default version.
    let audio = BRIDGE_PLUGINS
      .iter()
      .find(|s| s.crate_name == "openharmony-ability-plugin-audio")
      .unwrap();
    assert_eq!(audio.default_version, "^1.0.0-beta.4");
    // Verify all 9 core Tauri bridge plugins are marked core.
    assert_eq!(BRIDGE_PLUGINS.iter().filter(|s| s.core).count(), 9);
  }

  #[test]
  fn validate_identifier_accepts_official_and_local_ohpm_names() {
    assert!(validate_identifier("@tauri/plugin-dialog").is_ok());
    assert!(validate_identifier("my-local-plugin").is_ok());
    assert!(validate_identifier("@my-scope/plugin").is_ok());
    assert!(validate_identifier("plugin.name.2").is_ok());
  }

  #[test]
  fn validate_identifier_rejects_malformed_names() {
    assert!(validate_identifier("").is_err());
    assert!(validate_identifier("@scope").is_err());
    assert!(validate_identifier("@scope/").is_err());
    assert!(validate_identifier("has space").is_err());
    assert!(validate_identifier("../escape").is_err());
    assert!(validate_identifier("a'b").is_err());
    assert!(validate_identifier(".leading").is_err());
    assert!(validate_identifier("trailing.").is_err());
    assert!(validate_identifier("_under").is_err());
    assert!(validate_identifier("under_").is_err());
  }

  fn entry_ability(plugin_imports: &[&str], factories: &[&str]) -> Vec<String> {
    let mut lines = vec!["import { LazyPlugin, NativeAbility } from '@ohos-rs/ability';".to_string()];
    lines.extend(plugin_imports.iter().map(|l| l.to_string()));
    lines.extend([
      String::new(),
      "export default class EntryAbility extends NativeAbility {".to_string(),
      "  public bridgePlugins = [".to_string(),
    ]);
    lines.extend(
      factories
        .iter()
        .map(|n| format!("    new LazyPlugin(() => new {n}()),")),
    );
    lines.extend(["  ];".to_string(), "}".to_string()]);
    lines
  }

  #[test]
  fn backfills_missing_modular_bridge_factories_and_imports() {
    let all_specs: Vec<&BridgePluginSpec> = BRIDGE_PLUGINS.iter().collect();
    let mut lines = entry_ability(
      &["import { PermissionPlugin } from '@ohos-rs/ability-plugin-permission';"],
      &["PermissionPlugin"],
    );
    sync_bridge_plugin_factories(&mut lines, "entry_mobile", &all_specs);

    let content = lines.join("\n");
    assert!(content.contains("import { AccessibilityPlugin } from '@ohos-rs/ability-plugin-accessibility';"));
    assert!(content.contains("    new LazyPlugin(() => new AccessibilityPlugin()),"));
    assert!(content.contains(
      "import { GlobalShortcutPlugin as GlobalShortcutBridgePlugin } from '@ohos-rs/ability-plugin-global-shortcut';"
    ));
    assert!(content.contains("    new LazyPlugin(() => new GlobalShortcutBridgePlugin()),"));
    assert!(content.contains("import { WindowPlugin } from '@ohos-rs/ability-plugin-window';"));
    assert_eq!(
      content
        .matches("new LazyPlugin(() => new PermissionPlugin()),")
        .count(),
      1
    );
  }

  #[test]
  fn backfill_is_idempotent() {
    let all_specs: Vec<&BridgePluginSpec> = BRIDGE_PLUGINS.iter().collect();
    let mut lines = entry_ability(
      &["import { PermissionPlugin } from '@ohos-rs/ability-plugin-permission';"],
      &["PermissionPlugin"],
    );
    sync_bridge_plugin_factories(&mut lines, "entry_mobile", &all_specs);
    let once = lines.clone();
    sync_bridge_plugin_factories(&mut lines, "entry_mobile", &all_specs);
    assert_eq!(once, lines);
  }

  #[test]
  fn orphaned_import_is_not_duplicated() {
    let menu_spec = BRIDGE_PLUGINS
      .iter()
      .find(|s| s.ets_class_name == "MenuPlugin")
      .unwrap();
    let mut lines = entry_ability(
      &["import { MenuPlugin } from '@ohos-rs/ability-plugin-menu';"],
      &[],
    );
    sync_bridge_plugin_factories(&mut lines, "entry_mobile", &[menu_spec]);

    let content = lines.join("\n");
    assert_eq!(
      content
        .matches("import { MenuPlugin } from '@ohos-rs/ability-plugin-menu';")
        .count(),
      1
    );
    assert!(content.contains("new LazyPlugin(() => new MenuPlugin()),"));
  }

  #[test]
  fn pre_bridge_file_is_left_untouched() {
    let all_specs: Vec<&BridgePluginSpec> = BRIDGE_PLUGINS.iter().collect();
    let mut lines: Vec<String> = "export default class EntryAbility extends NativeAbility {"
      .lines()
      .map(String::from)
      .collect();
    let before = lines.clone();
    sync_bridge_plugin_factories(&mut lines, "entry_mobile", &all_specs);
    assert_eq!(before, lines);
  }

  #[test]
  fn selects_core_plus_manifest_extra_bridge_plugins_and_syncs_oh_package() {
    let corpus = r#"
[dependencies]
tauri-plugin-clipboard-manager = "2"
openharmony-ability-plugin-audio = "1.0.0-beta.4"
"#;
    let selected = select_bridge_plugins_for_manifest(corpus);
    assert!(selected.iter().any(|s| s.ohpm_package == "@ohos-rs/ability-plugin-window"));
    assert!(selected.iter().any(|s| s.ohpm_package == "@ohos-rs/ability-plugin-clipboard"));
    assert!(selected.iter().any(|s| s.ohpm_package == "@ohos-rs/ability-plugin-audio"));
    assert!(!selected.iter().any(|s| s.ohpm_package == "@ohos-rs/ability-plugin-updater"));

    let mut deps = serde_json::Map::new();
    let entry_dir = Path::new("/tmp/app/src-tauri/gen/ohos/entry_desktop");
    sync_bridge_plugin_dependencies(&mut deps, None, entry_dir, &selected);
    assert_eq!(deps["@ohos-rs/ability"].as_str(), Some("^1.0.0-beta.5"));
    assert_eq!(
      deps["@ohos-rs/ability-plugin-clipboard"].as_str(),
      Some("^1.0.0-beta.5")
    );
    assert_eq!(
      deps["@ohos-rs/ability-plugin-audio"].as_str(),
      Some("^1.0.0-beta.4")
    );
  }
}
