// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crate::error::Context;
use crate::{helpers::template, Result};
use cargo_mobile2::{
  config::app::App,
  open_harmony::{config::Config, target::Target},
  os,
  target::TargetTrait,
  util,
};
use handlebars::Handlebars;
use include_dir::{include_dir, Dir};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

use std::path::{Path, PathBuf};

use super::plugins::{PluginMeta, BRIDGE_PLUGINS};

const TEMPLATE_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/templates/mobile/open-harmony");

pub(crate) const ABILITY_PACKAGE_NAME: &str = "@ohos-rs/ability";
pub(crate) const ABILITY_DEFAULT_VERSION: &str = "^1.0.0-beta.5";

pub fn gen(
  app: &App,
  config: &Config,
  (handlebars, mut map): (Handlebars, template::JsonMap),
  skip_targets_install: bool,
  mobile_device_types: &str,
  desktop_device_types: &str,
) -> Result<()> {
  if !skip_targets_install {
    let installed_targets =
      crate::interface::rust::installation::installed_targets().unwrap_or_default();
    let missing_targets = Target::all()
      .values()
      .filter(|t| !installed_targets.contains(&t.triple().into()))
      .collect::<Vec<&Target>>();

    if !missing_targets.is_empty() {
      println!("Installing OpenHarmony Rust toolchains...");
      for target in missing_targets {
        target
          .install()
          .context("failed to install target with rustup")?;
      }
    }
  }

  println!("Generating DevEco Studio project...");
  let dest = config.project_dir();

  // `root-dir-rel` is consumed by every entry's `hvigorfile.ts`. Both
  // `entry_mobile` and `entry_desktop` sit at the same depth under the project
  // dir, so compute it once from `entry_mobile`.
  map.insert(
    "root-dir-rel",
    Path::new(&os::replace_path_separator(
      util::relativize_path(app.root_dir(), dest.join("entry_mobile")).into_os_string(),
    )),
  );
  map.insert("root-dir", app.root_dir());
  map.insert("windows", cfg!(windows));

  // Modular `@ohos-rs/ability` + `@ohos-rs/ability-plugin-*` resolution:
  // When a sibling `openharmony-ability` checkout (or `OHOS_ABILITY_DIR`) has
  // packed `.har` files under `dist/`, point oh-package.json5 at those local
  // HAR files via relative `file:...` paths. Otherwise default to OHPM
  // registry semver ranges (`^1.0.0-beta.5` / `^1.0.0-beta.4`).
  let ability_dir = find_sibling_ability_dir(&dest);
  let entry_ref_dir = dest.join("entry_desktop");

  let ability_dep = resolve_ability_dependency(
    ability_dir.as_deref(),
    &entry_ref_dir,
    ABILITY_PACKAGE_NAME,
    ABILITY_DEFAULT_VERSION,
  );
  map.insert("ability-dep", ability_dep.as_str());

  // When resolving local packed HARs from `<ability_dir>/dist`, plugin HARs
  // still declare `"@ohos-rs/ability": "workspace:*"` (and statusbar declares
  // `"@ohos-rs/ability-plugin-menu": "workspace:*"`). Emit root-level
  // `overrides` in `gen/ohos/oh-package.json5` so `ohpm install` resolves
  // those transitive `workspace:*` specs to the exact local HAR files.
  if let Some(ref dir) = ability_dir {
    if find_packed_har_in_dist(dir, ABILITY_PACKAGE_NAME).is_some() {
      let root_ability_override =
        resolve_ability_dependency(Some(dir), &dest, ABILITY_PACKAGE_NAME, ABILITY_DEFAULT_VERSION);
      map.insert("root-ability-override", root_ability_override);
      if find_packed_har_in_dist(dir, "@ohos-rs/ability-plugin-menu").is_some() {
        let root_menu_override = resolve_ability_dependency(
          Some(dir),
          &dest,
          "@ohos-rs/ability-plugin-menu",
          ABILITY_DEFAULT_VERSION,
        );
        map.insert("root-menu-override", root_menu_override);
      }
    }
  }

  for spec in BRIDGE_PLUGINS.iter().filter(|s| s.core) {
    let dep_value = resolve_ability_dependency(
      ability_dir.as_deref(),
      &entry_ref_dir,
      spec.ohpm_package,
      spec.default_version,
    );
    map.insert(&spec.template_dep_key(), dep_value);
  }

  // Render the shared (non-entry) parts of the tree with the global map. The
  // two entry modules are rendered separately below with per-form data.
  let mut skip_entries = |file_path: std::path::PathBuf| -> std::io::Result<Option<std::fs::File>> {
    let s = file_path.to_string_lossy();
    if s.starts_with("entry_mobile/") || s.starts_with("entry_desktop/") {
      return Ok(None);
    }
    let path = dest.join(&file_path);
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(path).map(Some)
  };
  template::render_with_generator(
    &handlebars,
    map.inner(),
    &TEMPLATE_DIR,
    &dest,
    &mut skip_entries,
  )
  .with_context(|| "failed to process template")?;

  // Render each entry module subtree with its form-specific data.
  for (form, device_types) in [
    ("mobile", mobile_device_types),
    ("desktop", desktop_device_types),
  ] {
    let module = format!("entry_{form}");
    let mut entry_data = map.inner().clone();
    entry_data.insert("form".into(), serde_json::Value::String(form.into()));
    entry_data.insert(
      "form-device-types".into(),
      serde_json::Value::String(device_types.into()),
    );
    let entry_dir = TEMPLATE_DIR
      .get_dir(&module)
      .with_context(|| format!("template dir {module} not found"))?;
    template::render(&handlebars, &entry_data, entry_dir, &dest)
      .with_context(|| format!("failed to render {module}"))?;
  }

  inject_ohos_cargo_dependencies(app.root_dir(), ability_dir.as_deref())?;

  Ok(())
}

pub fn gen_with_plugins(
  app: &App,
  config: &Config,
  (handlebars, mut map): (Handlebars, template::JsonMap),
  skip_targets_install: bool,
  plugin_metadata: Vec<PluginMeta>,
  mobile_device_types: &str,
  desktop_device_types: &str,
) -> Result<()> {
  if !plugin_metadata.is_empty() {
    let plugin_list: Vec<serde_json::Value> = plugin_metadata
      .iter()
      .map(|p| {
        serde_json::json!({
          "name": p.name,
          "identifier": p.identifier,
          "className": p.class_name,
        })
      })
      .collect();
    map.insert("plugins", &plugin_list);
  }

  gen(
    app,
    config,
    (handlebars, map),
    skip_targets_install,
    mobile_device_types,
    desktop_device_types,
  )
}

/// Checks whether `dir` looks like an `openharmony-ability` checkout: either
/// it has packed modular HARs in `dist/` (`ohos-rs-ability-*.har`) or it
/// contains `native_ability/oh-package.json5`.
fn is_valid_ability_dir(dir: &Path) -> bool {
  find_packed_har_in_dist(dir, ABILITY_PACKAGE_NAME).is_some()
    || dir
      .join("native_ability")
      .join("oh-package.json5")
      .is_file()
}

/// Discover a sibling `openharmony-ability` checkout directory.
///
/// Resolution order:
/// 1. `OHOS_ABILITY_DIR` environment variable (when pointing at a valid directory).
/// 2. Walk up ancestors from `start` looking for a sibling `openharmony-ability/`
///    directory containing `dist/ohos-rs-ability-*.har` or
///    `native_ability/oh-package.json5`.
pub(crate) fn find_sibling_ability_dir(start: &Path) -> Option<PathBuf> {
  if let Some(env_dir) = std::env::var_os("OHOS_ABILITY_DIR") {
    let candidate = PathBuf::from(&env_dir);
    if is_valid_ability_dir(&candidate) {
      return Some(candidate);
    }
    log::warn!(
      "ignoring OHOS_ABILITY_DIR={}: expected dist/ohos-rs-ability-*.har or native_ability/oh-package.json5",
      candidate.display()
    );
  }

  find_sibling_ability_dir_from_ancestors(start)
}

fn find_sibling_ability_dir_from_ancestors(start: &Path) -> Option<PathBuf> {
  let mut current = start;
  loop {
    let candidate = current.join("openharmony-ability");
    if is_valid_ability_dir(&candidate) {
      return Some(candidate);
    }
    match current.parent() {
      Some(parent) => current = parent,
      None => return None,
    }
  }
}

/// Converts an OHPM package name into the packed HAR filename prefix produced
/// by `openharmony-ability`'s `pnpm run pack` (`dist/<prefix><version>.har`).
///
/// Examples:
/// - `@ohos-rs/ability` -> `ohos-rs-ability-`
/// - `@ohos-rs/ability-plugin-window` -> `ohos-rs-ability-plugin-window-`
fn package_to_har_prefix(package_name: &str) -> String {
  let stripped = package_name.trim_start_matches('@').replace('/', "-");
  format!("{stripped}-")
}

/// Locates a packed modular `.har` for `package_name` inside `<ability_dir>/dist`.
///
/// Requires the character immediately following `<prefix>` to be an ASCII digit
/// (semver major) so `@ohos-rs/ability` (`ohos-rs-ability-1.0.0-beta.5.har`)
/// never falsely matches plugin HARs (`ohos-rs-ability-plugin-*.har`).
pub(crate) fn find_packed_har_in_dist(ability_dir: &Path, package_name: &str) -> Option<PathBuf> {
  let dist_dir = ability_dir.join("dist");
  let entries = std::fs::read_dir(&dist_dir).ok()?;
  let prefix = package_to_har_prefix(package_name);

  let mut matches: Vec<PathBuf> = entries
    .filter_map(|e| e.ok())
    .map(|e| e.path())
    .filter(|path| {
      if !path.is_file() {
        return false;
      }
      let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
      };
      let Some(rest) = file_name.strip_prefix(&prefix) else {
        return false;
      };
      rest.ends_with(".har") && rest.as_bytes().first().is_some_and(u8::is_ascii_digit)
    })
    .collect();

  matches.sort();
  matches.pop()
}

/// Resolves the `oh-package.json5` dependency value for `@ohos-rs/ability` or
/// any `@ohos-rs/ability-plugin-*` package.
///
/// Returns `"file:<relative-path-to-har>"` when a local checkout with packed
/// `dist/*.har` files is available, and falls back to `default_version`
/// (OHPM registry semver) otherwise.
pub(crate) fn resolve_ability_dependency(
  ability_dir: Option<&Path>,
  entry_dir: &Path,
  package_name: &str,
  default_version: &str,
) -> String {
  if let Some(har_path) = ability_dir.and_then(|dir| find_packed_har_in_dist(dir, package_name)) {
    let rel = os::replace_path_separator(
      util::relativize_path(har_path, entry_dir).into_os_string(),
    )
    .to_string_lossy()
    .into_owned();
    format!("file:{rel}")
  } else {
    default_version.to_string()
  }
}

/// The `cfg(target_env = "ohos")` key inside `[target]`.
const OHOS_TARGET_TABLE_KEY: &str = r#"cfg(target_env = "ohos")"#;

/// Entries injected into `[target.'cfg(target_env = "ohos")'.dependencies]`:
/// the `tauri::ohos::entry_point` / `tauri::mobile_entry_point` expansion on
/// OHOS references `napi_ohos`, `napi_derive_ohos`, `::openharmony_ability`,
/// and `::log` paths directly.
const OHOS_TARGET_DEPENDENCIES: [(&str, &str); 4] = [
  ("napi-ohos", "1.2"),
  ("napi-derive-ohos", "1.2"),
  ("openharmony-ability", "1.0.0-beta.5"),
  ("log", "0.4"),
];

/// Collect sibling `openharmony-ability` Rust crate directories that exist under
/// `ability_dir/crates/` for `[patch.crates-io]`: always includes
/// `openharmony-ability` and `openharmony-ability-derive`, plus any
/// `openharmony-ability-plugin-*` crate referenced in `manifest_text`.
fn sibling_ability_crates(ability_dir: &Path, manifest_text: &str) -> Vec<(&'static str, PathBuf)> {
  let mut crates = Vec::new();
  for (crate_name, subdir) in [
    ("openharmony-ability", "ability"),
    ("openharmony-ability-derive", "derive"),
  ] {
    let dir = ability_dir.join("crates").join(subdir);
    if dir.join("Cargo.toml").is_file() {
      crates.push((crate_name, dir));
    }
  }

  for spec in BRIDGE_PLUGINS {
    if !manifest_text.contains(spec.crate_name) {
      continue;
    }
    let Some(suffix) = spec.crate_name.strip_prefix("openharmony-ability-") else {
      continue;
    };
    let dir = ability_dir.join("crates").join(suffix);
    if dir.join("Cargo.toml").is_file() {
      crates.push((spec.crate_name, dir));
    }
  }

  crates
}

/// Nearest ancestor `Cargo.toml` declaring a `[workspace]` table, when the
/// app manifest at `tauri_dir` is not itself a workspace root.
fn find_owning_workspace_manifest(tauri_dir: &Path, doc: &DocumentMut) -> Option<PathBuf> {
  if doc.as_table().contains_key("workspace") {
    return None;
  }
  let mut current = tauri_dir.parent()?;
  loop {
    let candidate = current.join("Cargo.toml");
    if candidate.is_file() {
      if let Ok(text) = std::fs::read_to_string(&candidate) {
        if let Ok(ancestor) = text.parse::<DocumentMut>() {
          if ancestor.as_table().contains_key("workspace") {
            return Some(candidate);
          }
        }
      }
    }
    current = current.parent()?;
  }
}

/// What [`apply_ohos_cargo_dependencies`] changed, for reporting and tests.
struct OhosCargoInjectionReport {
  added_dependencies: Vec<&'static str>,
  added_patches: Vec<(&'static str, String)>,
}

/// Pure TOML-merge half of [`inject_ohos_cargo_dependencies`] (no I/O): inject
/// missing `[target.'cfg(target_env = "ohos")'.dependencies]` keys and any
/// provided `[patch.crates-io]` entries without rewriting existing keys.
fn apply_ohos_cargo_dependencies(
  mut doc: DocumentMut,
  ability_patches: &[(&'static str, String)],
) -> Result<(String, OhosCargoInjectionReport)> {
  let root = doc.as_table_mut();

  let target = implicit_table_entry(root, "target")?;
  let ohos = implicit_table_entry(target, OHOS_TARGET_TABLE_KEY)?;
  let dependencies = explicit_table_entry(ohos, "dependencies")?;

  let mut added_dependencies = Vec::new();
  for (name, requirement) in OHOS_TARGET_DEPENDENCIES {
    let item = dependencies.entry(name).or_insert(Item::None);
    if matches!(item, Item::None) {
      *item = Item::Value(requirement.into());
      added_dependencies.push(name);
    }
  }

  let mut added_patches = Vec::new();
  if !ability_patches.is_empty() {
    let patch = implicit_table_entry(root, "patch")?;
    let crates_io = explicit_table_entry(patch, "crates-io")?;
    for (crate_name, patch_path) in ability_patches {
      let item = crates_io.entry(crate_name).or_insert(Item::None);
      if matches!(item, Item::None) {
        let mut definition = InlineTable::new();
        definition.get_or_insert("path", patch_path.clone());
        *item = Item::Value(Value::InlineTable(definition));
        added_patches.push((*crate_name, patch_path.clone()));
      }
    }
  }

  Ok((
    doc.to_string(),
    OhosCargoInjectionReport {
      added_dependencies,
      added_patches,
    },
  ))
}

fn implicit_table() -> Item {
  let mut table = Table::new();
  table.set_implicit(true);
  Item::Table(table)
}

fn explicit_table() -> Item {
  Item::Table(Table::new())
}

fn implicit_table_entry<'a>(parent: &'a mut Table, key: &str) -> Result<&'a mut Table> {
  parent
    .entry(key)
    .or_insert(implicit_table())
    .as_table_mut()
    .with_context(|| format!("`{key}` in Cargo.toml exists but is not a table"))
}

fn explicit_table_entry<'a>(parent: &'a mut Table, key: &str) -> Result<&'a mut Table> {
  parent
    .entry(key)
    .or_insert(explicit_table())
    .as_table_mut()
    .with_context(|| format!("`{key}` in Cargo.toml exists but is not a table"))
}

/// Ensure the app's `Cargo.toml` carries the OHOS target dependencies needed by
/// `tauri::ohos::entry_point`, and optionally patches sibling `openharmony-ability`
/// crates when a local checkout is discovered beside the project.
fn inject_ohos_cargo_dependencies(tauri_dir: &Path, ability_dir: Option<&Path>) -> Result<()> {
  let manifest_path = tauri_dir.join("Cargo.toml");
  let manifest = std::fs::read_to_string(&manifest_path)
    .with_context(|| format!("failed to read {}", manifest_path.display()))?;
  let doc = manifest
    .parse::<DocumentMut>()
    .with_context(|| format!("failed to parse {} as TOML", manifest_path.display()))?;

  let mut patches: Vec<(&'static str, String)> = Vec::new();
  if let Some(dir) = ability_dir {
    let sibling_crates = sibling_ability_crates(dir, &manifest);
    if !sibling_crates.is_empty() {
      if let Some(workspace_root) = find_owning_workspace_manifest(tauri_dir, &doc) {
        log::warn!(
          "the app manifest is a member of the workspace at {} — cargo only honors \
           `[patch.crates-io]` in the workspace root, so local `openharmony-ability` \
           patches were not written into the app manifest",
          workspace_root.display()
        );
      } else {
        for (crate_name, crate_dir) in sibling_crates {
          let rel = util::relativize_path(crate_dir, tauri_dir)
            .to_string_lossy()
            .replace('\\', "/");
          patches.push((crate_name, rel));
        }
      }
    }
  } else {
    log::info!(
      "no sibling openharmony-ability checkout found — OHOS oh-package.json5 uses \
       OHPM registry versions ({ABILITY_DEFAULT_VERSION}); set OHOS_ABILITY_DIR to \
       use local packed HARs"
    );
  }

  let (updated, report) = apply_ohos_cargo_dependencies(doc, &patches)?;

  if report.added_dependencies.is_empty() && report.added_patches.is_empty() {
    return Ok(());
  }
  std::fs::write(&manifest_path, updated)
    .with_context(|| format!("failed to write {}", manifest_path.display()))?;
  if !report.added_dependencies.is_empty() {
    println!(
      "Added OHOS target dependencies to {}: {}",
      manifest_path.display(),
      report.added_dependencies.join(", ")
    );
  }
  if !report.added_patches.is_empty() {
    let names: Vec<&str> = report.added_patches.iter().map(|(n, _)| *n).collect();
    println!(
      "Patched local openharmony-ability crates in {}: {}",
      manifest_path.display(),
      names.join(", ")
    );
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  const OHOS_TABLE_HEADER: &str = "[target.'cfg(target_env = \"ohos\")'.dependencies]";

  #[test]
  fn injects_all_dependencies_into_a_fresh_manifest() {
    let (updated, report) = apply_ohos_cargo_dependencies(
      "[package]\nname = \"app\"\nversion = \"0.1.0\"\n"
        .parse()
        .unwrap(),
      &[],
    )
    .unwrap();

    assert_eq!(
      report.added_dependencies.len(),
      OHOS_TARGET_DEPENDENCIES.len()
    );
    assert!(report.added_patches.is_empty());
    assert_eq!(updated.matches(OHOS_TABLE_HEADER).count(), 1);

    let doc: DocumentMut = updated.parse().unwrap();
    let deps = doc["target"][OHOS_TARGET_TABLE_KEY]["dependencies"]
      .as_table()
      .unwrap();
    for (name, requirement) in OHOS_TARGET_DEPENDENCIES {
      assert_eq!(deps[name].as_str(), Some(requirement), "`{name}` missing");
    }
  }

  #[test]
  fn re_running_is_a_noop() {
    let (once, report) =
      apply_ohos_cargo_dependencies("[package]\nname = \"app\"\n".parse().unwrap(), &[]).unwrap();
    assert!(!report.added_dependencies.is_empty());

    let (twice, report) = apply_ohos_cargo_dependencies(once.parse().unwrap(), &[]).unwrap();
    assert!(report.added_dependencies.is_empty());
    assert!(report.added_patches.is_empty());
    assert_eq!(once, twice);
  }

  #[test]
  fn merges_into_an_existing_table_without_duplicating_it() {
    let manifest = concat!(
      "[package]\nname = \"app\"\n\n",
      "[target.'cfg(target_env = \"ohos\")'.dependencies]\n",
      "openharmony-ability = { path = \"../../my-fork/crates/ability\" }\n\n",
      "[target.'cfg(windows)'.dependencies]\nwinapi = \"0.3\"\n",
    );
    let (updated, report) = apply_ohos_cargo_dependencies(manifest.parse().unwrap(), &[]).unwrap();

    assert_eq!(report.added_dependencies.len(), 3);
    assert_eq!(updated.matches(OHOS_TABLE_HEADER).count(), 1);
    assert!(
      updated.contains("openharmony-ability = { path = \"../../my-fork/crates/ability\" }"),
      "user entry must be preserved verbatim"
    );
    assert!(updated.contains("winapi = \"0.3\""));

    let doc: DocumentMut = updated.parse().unwrap();
    let deps = doc["target"][OHOS_TARGET_TABLE_KEY]["dependencies"]
      .as_table()
      .unwrap();
    assert!(deps.contains_key("openharmony-ability"));
    for (name, requirement) in OHOS_TARGET_DEPENDENCIES {
      if name != "openharmony-ability" {
        assert_eq!(deps[name].as_str(), Some(requirement));
      }
    }
  }

  #[test]
  fn a_manifest_that_mentions_the_crate_elsewhere_still_gets_the_keys() {
    let manifest = concat!(
      "# openharmony-ability is consumed via the sibling checkout\n",
      "[package]\nname = \"app\"\n\n",
      "[dependencies]\nserde = \"1\"\n",
    );
    let (updated, report) = apply_ohos_cargo_dependencies(manifest.parse().unwrap(), &[]).unwrap();
    assert_eq!(report.added_dependencies.len(), 4);
    assert!(updated.contains("napi-ohos = \"1.2\""));
    assert!(updated.contains("openharmony-ability = \"1.0.0-beta.5\""));
    assert!(updated.contains("log = \"0.4\""));
  }

  #[test]
  fn injects_the_sibling_patches_beside_existing_patch_entries() {
    let manifest = concat!(
      "[package]\nname = \"app\"\n\n",
      "[patch.crates-io]\nmy-crate = { path = \"../my-crate\" }\n",
    );
    let patches = [
      (
        "openharmony-ability",
        "../../openharmony-ability/crates/ability".to_string(),
      ),
      (
        "openharmony-ability-derive",
        "../../openharmony-ability/crates/derive".to_string(),
      ),
    ];
    let (updated, report) =
      apply_ohos_cargo_dependencies(manifest.parse().unwrap(), &patches).unwrap();

    assert_eq!(report.added_dependencies.len(), 4);
    assert_eq!(report.added_patches.len(), 2);
    assert!(updated.contains("my-crate = { path = \"../my-crate\" }"));
    assert!(updated
      .contains("openharmony-ability = { path = \"../../openharmony-ability/crates/ability\" }"));
    assert!(updated.contains(
      "openharmony-ability-derive = { path = \"../../openharmony-ability/crates/derive\" }"
    ));

    let doc: DocumentMut = updated.parse().unwrap();
    assert_eq!(
      doc["patch"]["crates-io"]["openharmony-ability"]["path"].as_str(),
      Some("../../openharmony-ability/crates/ability")
    );
    assert_eq!(
      doc["patch"]["crates-io"]["openharmony-ability-derive"]["path"].as_str(),
      Some("../../openharmony-ability/crates/derive")
    );
  }

  #[test]
  fn keeps_an_existing_openharmony_ability_patch() {
    let manifest = concat!(
      "[package]\nname = \"app\"\n\n",
      "[patch.crates-io]\nopenharmony-ability = { path = \"../my-fork/crates/ability\" }\n",
    );
    let patches = [(
      "openharmony-ability",
      "../../openharmony-ability/crates/ability".to_string(),
    )];
    let (updated, report) =
      apply_ohos_cargo_dependencies(manifest.parse().unwrap(), &patches).unwrap();

    assert!(report.added_patches.is_empty());
    assert!(updated.contains("../my-fork/crates/ability"));
    assert!(!updated.contains("../../openharmony-ability/crates/ability"));
  }

  #[test]
  fn modular_har_discovery_distinguishes_core_from_plugins_and_falls_back_to_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let ability_dir = tmp.path().join("openharmony-ability");
    let dist_dir = ability_dir.join("dist");
    std::fs::create_dir_all(&dist_dir).unwrap();
    std::fs::write(dist_dir.join("ohos-rs-ability-1.0.0-beta.5.har"), b"core").unwrap();
    std::fs::write(
      dist_dir.join("ohos-rs-ability-plugin-window-1.0.0-beta.5.har"),
      b"window",
    )
    .unwrap();
    std::fs::write(
      dist_dir.join("ohos-rs-ability-plugin-audio-1.0.0-beta.4.har"),
      b"audio",
    )
    .unwrap();

    let entry_dir = tmp
      .path()
      .join("my-app")
      .join("src-tauri")
      .join("gen")
      .join("ohos")
      .join("entry_desktop");
    std::fs::create_dir_all(&entry_dir).unwrap();

    assert_eq!(
      find_sibling_ability_dir_from_ancestors(&entry_dir),
      Some(ability_dir.clone())
    );

    // Core package must match ohos-rs-ability-1.0.0-beta.5.har, NOT plugin HARs.
    let core_dep = resolve_ability_dependency(
      Some(&ability_dir),
      &entry_dir,
      ABILITY_PACKAGE_NAME,
      ABILITY_DEFAULT_VERSION,
    );
    assert_eq!(
      core_dep,
      "file:../../../../../openharmony-ability/dist/ohos-rs-ability-1.0.0-beta.5.har"
    );

    let window_dep = resolve_ability_dependency(
      Some(&ability_dir),
      &entry_dir,
      "@ohos-rs/ability-plugin-window",
      "^1.0.0-beta.5",
    );
    assert_eq!(
      window_dep,
      "file:../../../../../openharmony-ability/dist/ohos-rs-ability-plugin-window-1.0.0-beta.5.har"
    );

    // Unpacked plugin falls back to registry semver.
    let missing_dep = resolve_ability_dependency(
      Some(&ability_dir),
      &entry_dir,
      "@ohos-rs/ability-plugin-webview",
      "^1.0.0-beta.5",
    );
    assert_eq!(missing_dep, "^1.0.0-beta.5");

    // No local checkout falls back to registry semver (including audio beta.4).
    assert_eq!(
      resolve_ability_dependency(
        None,
        &entry_dir,
        "@ohos-rs/ability-plugin-audio",
        "^1.0.0-beta.4"
      ),
      "^1.0.0-beta.4"
    );
  }
}
