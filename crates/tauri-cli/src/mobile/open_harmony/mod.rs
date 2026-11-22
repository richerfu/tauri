// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use serde::Deserialize;

use cargo_mobile2::{
  config::app::App,
  open_harmony::{
    config::{
      Config as OpenHarmonyConfig, Metadata as OpenHarmonyMetadata, Raw as RawOpenHarmonyConfig,
    },
    device::Device,
    emulator,
    env::Env,
    hdc,
    target::Target,
  },
  opts::{FilterLevel, NoiseLevel},
  os,
  util::prompt,
};
use clap::{Parser, Subcommand};
use std::{
  env::{set_var, var},
  fs::{create_dir_all, write},
  path::PathBuf,
  thread::sleep,
  time::Duration,
};
use sublime_fuzzy::best_match;
use tauri_utils::config::OpenHarmonyDeviceTypes;
use tauri_utils::resources::ResourcePaths;

use super::{
  ensure_init, get_app, init, log_finished, read_options, CliOptions, OptionsHandle,
  Target as MobileTarget, MIN_DEVICE_MATCH_SCORE,
};
use crate::error::Context;
use crate::{
  helpers::config::{BundleResources, Config as TauriConfig},
  ConfigValue, ErrorExt, Result,
};

mod build;
mod dev;
mod dev_eco_studio_script;
pub(crate) mod plugins;
pub(crate) mod project;
mod run;
pub(crate) mod signing;

#[derive(Deserialize)]
pub struct AppConfig {
  pub app: AppConfigObject,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfigObject {
  pub bundle_name: String,
  // TODO: impl versioning
  //pub version_code: u32,
  //pub version_name: String,
}

#[derive(Parser)]
#[clap(
  author,
  version,
  about = "OpenHarmony commands",
  subcommand_required(true),
  arg_required_else_help(true)
)]
pub struct Cli {
  #[clap(subcommand)]
  command: Commands,
}

#[derive(Debug, Parser)]
#[clap(about = "Initialize OpenHarmony target in the project")]
pub struct InitOptions {
  /// Skip prompting for values
  #[clap(long, env = "CI")]
  ci: bool,
  /// Skips installing rust toolchains via rustup
  #[clap(long)]
  skip_targets_install: bool,
  /// JSON strings or paths to JSON, JSON5 or TOML files to merge with the default configuration file
  ///
  /// Configurations are merged in the order they are provided, which means a particular value overwrites previous values when a config key-value pair conflicts.
  ///
  /// Note that a platform-specific file is looked up and merged with the default file by default
  /// (tauri.macos.conf.json, tauri.linux.conf.json, tauri.windows.conf.json, tauri.android.conf.json, tauri.ios.conf.json and tauri.ohos.conf.json)
  /// but you can use this for more specific use cases such as different build flavors.
  #[clap(short, long)]
  pub config: Vec<ConfigValue>,
}

#[derive(Subcommand)]
enum Commands {
  Init(InitOptions),
  Dev(dev::Options),
  Build(build::Options),
  Run(run::Options),
  #[clap(hide(true))]
  DevEcoStudioScript(dev_eco_studio_script::Options),
}

pub fn command(cli: Cli, verbosity: u8) -> Result<()> {
  let noise_level = NoiseLevel::from_occurrences(verbosity as u64);
  match cli.command {
    Commands::Init(options) => {
      crate::helpers::app_paths::resolve_dirs();
      init::command(
        MobileTarget::OpenHarmony,
        options.ci,
        false,
        options.skip_targets_install,
        options.config,
      )?
    }
    Commands::Dev(options) => dev::command(options, noise_level)?,
    Commands::Build(options) => build::command(options, noise_level)?,
    Commands::Run(options) => run::command(options, noise_level)?,
    Commands::DevEcoStudioScript(options) => dev_eco_studio_script::command(options)?,
  }

  Ok(())
}

pub fn get_config(
  app: &App,
  _config: &TauriConfig,
  features: Option<&Vec<String>>,
  cli_options: &CliOptions,
) -> (OpenHarmonyConfig, OpenHarmonyMetadata) {
  let mut open_harmony_options = cli_options.clone();
  if let Some(features) = features {
    open_harmony_options.features.extend_from_slice(features);
  }

  let raw = RawOpenHarmonyConfig {
    features: Some(open_harmony_options.features.clone()),
    logcat_filter_specs: vec![
      "RustStdoutStderr".into(),
      format!(
        "*:{}",
        match cli_options.noise_level {
          NoiseLevel::Polite => FilterLevel::Info,
          NoiseLevel::LoudAndProud => FilterLevel::Debug,
          NoiseLevel::FranklyQuitePedantic => FilterLevel::Verbose,
        }
        .logcat()
      ),
    ],
    ..Default::default()
  };
  let config = OpenHarmonyConfig::from_raw(app.clone(), Some(raw)).unwrap();

  let metadata = OpenHarmonyMetadata {
    supported: true,
    cargo_args: Some(open_harmony_options.args),
    features: Some(open_harmony_options.features),
    ..Default::default()
  };

  set_var(
    "WRY_OHOS_PACKAGE",
    app.android_identifier_escape_kotlin_keyword(),
  );
  set_var("TAURI_OHOS_PACKAGE_UNESCAPED", app.identifier());
  set_var("WRY_OHOS_LIBRARY", app.lib_name());
  set_var("TAURI_OHOS_PROJECT_PATH", config.project_dir());

  // Also set device type from cli_options.vars if present
  if let Some(device_type) = cli_options.vars.get("OHOS_DEVICE_TYPE") {
    set_var("OHOS_DEVICE_TYPE", device_type);
  }

  (config, metadata)
}

fn env() -> Result<Env> {
  let env = super::env().context("failed to setup OpenHarmony environment")?;
  cargo_mobile2::open_harmony::env::Env::from_env(env).context("failed to OpenHarmony load env")
}

fn delete_codegen_vars() {
  for (k, _) in std::env::vars() {
    if k.starts_with("WRY_") && (k.ends_with("CLASS_EXTENSION") || k.ends_with("CLASS_INIT")) {
      std::env::remove_var(k);
    }
  }
}

fn hdc_device_prompt<'a>(env: &'_ Env, target: Option<&str>) -> Result<Device<'a>> {
  let device_list =
    hdc::device_list(env).context("failed to detect connected OpenHarmony devices")?;
  if !device_list.is_empty() {
    let device = if let Some(t) = target {
      let (device, score) = device_list
        .into_iter()
        .rev()
        .map(|d| {
          let score = best_match(t, d.name()).map_or(0, |m| m.score());
          (d, score)
        })
        .max_by_key(|(_, score)| *score)
        // we already checked the list is not empty
        .unwrap();
      if score > MIN_DEVICE_MATCH_SCORE {
        device
      } else {
        crate::error::bail!("Could not find an OpenHarmony device matching {t}")
      }
    } else if device_list.len() > 1 {
      let index = prompt::list(
        concat!("Detected ", "OpenHarmony", " devices"),
        device_list.iter(),
        "device",
        None,
        "Device",
      )
      .context("Failed to prompt for OpenHarmony devices")?;
      device_list.into_iter().nth(index).unwrap()
    } else {
      device_list.into_iter().next().unwrap()
    };

    log::info!(
      "Detected connected device: {} with target {:?}",
      device,
      device.target().triple,
    );
    Ok(device)
  } else {
    Err(crate::Error::GenericError(
      "No connected OpenHarmony devices detected".to_string(),
    ))
  }
}

fn emulator_prompt(_env: &'_ Env, target: Option<&str>) -> Result<emulator::Emulator> {
  let emulator_list = emulator::hvd_list().unwrap_or_default();
  if !emulator_list.is_empty() {
    let emulator = if let Some(t) = target {
      let (device, score) = emulator_list
        .into_iter()
        .rev()
        .map(|d| {
          let score = best_match(t, d.name()).map_or(0, |m| m.score());
          (d, score)
        })
        .max_by_key(|(_, score)| *score)
        // we already checked the list is not empty
        .unwrap();
      if score > MIN_DEVICE_MATCH_SCORE {
        device
      } else {
        crate::error::bail!("Could not find an OpenHarmony Emulator matching {t}")
      }
    } else if emulator_list.len() > 1 {
      let index = prompt::list(
        concat!("Detected ", "OpenHarmony", " emulators"),
        emulator_list.iter(),
        "emulator",
        None,
        "Emulator",
      )
      .context("Failed to prompt for OpenHarmony Emulator device")?;
      emulator_list.into_iter().nth(index).unwrap()
    } else {
      emulator_list.into_iter().next().unwrap()
    };

    Ok(emulator)
  } else {
    Err(crate::Error::GenericError(
      "No available OpenHarmony Emulator detected".to_string(),
    ))
  }
}

fn device_prompt<'a>(env: &'_ Env, target: Option<&str>) -> Result<Device<'a>> {
  if let Ok(device) = hdc_device_prompt(env, target) {
    Ok(device)
  } else {
    let emulator = emulator_prompt(env, target)?;
    log::info!("Starting emulator {}", emulator.name());
    emulator
      .start_detached(env)
      .context("failed to start emulator")?;
    let mut tries = 0;
    loop {
      sleep(Duration::from_secs(2));
      if let Ok(device) = hdc_device_prompt(env, Some(emulator.name())) {
        return Ok(device);
      }
      if tries >= 3 {
        log::info!("Waiting for emulator to start... (maybe the emulator is unauthorized or offline, run `hdc list targets` to check)");
      } else {
        log::info!("Waiting for emulator to start...");
      }
      tries += 1;
    }
  }
}

fn detect_target_ok<'a>(env: &Env) -> Option<&'a Target<'a>> {
  device_prompt(env, None).map(|device| device.target()).ok()
}

fn open_and_wait(config: &OpenHarmonyConfig, env: &Env) -> ! {
  log::info!("Opening DevEco Studio");
  // "DevEco Studio" (with a space) is the actual application name — on macOS
  // `open -a` only accepts the real app name.
  if let Err(e) = os::open_file_with("DevEco Studio", config.project_dir(), &env.base) {
    log::error!("{e}");
  }
  loop {
    sleep(Duration::from_secs(24 * 60 * 60));
  }
}

/// The active entry module name (`entry_mobile` / `entry_desktop`), driven by
/// `OHOS_DEVICE_TYPE` (set by the CLI build/dev commands). Falls back to
/// `entry_mobile` when unset. Used by the build-time injectors (icons, plugin
/// oh-package deps) and build-profile module selection to target the entry
/// being built.
pub fn active_entry_module() -> String {
  let form = var("OHOS_DEVICE_TYPE").unwrap_or_else(|_| "mobile".to_string());
  format!("entry_{form}")
}

/// The conf `deviceTypes` list for the given form. With the per-form config
/// schema (`{ mobile: [...], desktop: [...]] }`), this is a direct lookup — no
/// intersection with a hardcoded device-class set.
pub fn device_types_for_form(device_types: &OpenHarmonyDeviceTypes, form: &str) -> Vec<String> {
  match form {
    "mobile" => device_types.mobile.clone(),
    "desktop" => device_types.desktop.clone(),
    _ => Vec::new(),
  }
}

/// Active device forms: `mobile` if its list is non-empty, `desktop` if its
/// list is non-empty. Used by `build --app` to decide which entry modules to
/// compile and package.
pub fn forms_for_device_types(device_types: &OpenHarmonyDeviceTypes) -> Vec<&'static str> {
  let mut forms = Vec::new();
  if !device_types.mobile.is_empty() {
    forms.push("mobile");
  }
  if !device_types.desktop.is_empty() {
    forms.push("desktop");
  }
  forms
}

/// Copy `tauri.conf.json` and the configured `bundle.resources` into the
/// `entry_{form}` module's `src/main/resources/resfile/` directory — the
/// app-writable location besides `AppScope/resources` that the HAP actually
/// packages (a top-level `assets/` dir would be silently dropped).
///
/// `resfile` (not `rawfile`) is required for `resource_dir()` to work: the
/// system extracts a module's `resources/resfile` HAP content into the
/// directly readable `/data/storage/el1/bundle/<module>/resources/resfile`
/// directory (the official `resourceDir` location, matching the Rust path
/// resolver), while `rawfile` content stays inside the zipped HAP and is only
/// reachable through the resource manager's rawfile APIs.
fn inject_resources(
  config: &OpenHarmonyConfig,
  tauri_config: &TauriConfig,
  form: &str,
) -> Result<()> {
  let resfile_dir = config
    .project_dir()
    .join(format!("entry_{form}/src/main/resources/resfile"));

  create_dir_all(&resfile_dir)
    .fs_context("failed to create resfile directory", resfile_dir.clone())?;

  write(
    resfile_dir.join("tauri.conf.json"),
    serde_json::to_string(&tauri_config).with_context(|| "failed to serialize tauri config")?,
  )
  .fs_context(
    "failed to write tauri config",
    resfile_dir.join("tauri.conf.json"),
  )?;

  let resources = match &tauri_config.bundle.resources {
    Some(BundleResources::List(paths)) => Some(ResourcePaths::new(paths.as_slice(), true)),
    Some(BundleResources::Map(map)) => Some(ResourcePaths::from_map(map, true)),
    None => None,
  };
  if let Some(resources) = resources {
    for resource in resources.iter() {
      let resource = resource.context("failed to get resource")?;
      let dest = resfile_dir.join(resource.target());
      crate::helpers::fs::copy_file(resource.path(), dest)?;
    }
  }

  Ok(())
}

/// Minimal valid 256x256 opaque white (`#FFFFFFFF`) RGBA PNG used as the
/// HarmonyOS `layered_image` background layer when an app only supplies
/// standard desktop icons in `bundle.icon` (no explicit `*-background.png`).
/// Keeping an opaque background avoids HarmonyOS rendering transparent corners
/// as black in the launcher squircle and replaces DevEco's default blue tile.
const FALLBACK_BACKGROUND_PNG: &[u8] = &[
  0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
  0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x08, 0x06, 0x00, 0x00, 0x00, 0x5c, 0x72, 0xa8,
  0x66, 0x00, 0x00, 0x02, 0x59, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0xed, 0xd4, 0x31, 0x01, 0x00,
  0x00, 0x0c, 0xc2, 0x30, 0xfc, 0x9b, 0x06, 0x03, 0x73, 0xb0, 0x44, 0x42, 0x8f, 0xa6, 0xc0, 0x5b,
  0x91, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0,
  0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30,
  0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c,
  0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03,
  0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00,
  0x00, 0x03, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00,
  0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00,
  0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00,
  0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0,
  0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30,
  0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03,
  0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00,
  0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00,
  0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00,
  0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00,
  0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0,
  0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30,
  0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c,
  0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03,
  0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00,
  0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x03, 0x00, 0x0c, 0x00,
  0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00,
  0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00,
  0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0,
  0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30,
  0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x03,
  0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00,
  0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00,
  0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00,
  0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00,
  0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00,
  0x03, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30,
  0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c,
  0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03,
  0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00,
  0x00, 0x03, 0x00, 0x0c, 0x00, 0x30, 0x00, 0xc0, 0x00, 0x00, 0x03, 0x00, 0x2e, 0x03, 0xe6, 0x50,
  0x3b, 0xc5, 0xd1, 0x06, 0x25, 0x07, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42,
  0x60, 0x82,
];

fn read_png_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
  use std::io::Read;
  let mut file = std::fs::File::open(path).ok()?;
  let mut header = [0u8; 24];
  file.read_exact(&mut header).ok()?;
  if &header[0..8] != b"\x89PNG\r\n\x1a\n" || &header[12..16] != b"IHDR" {
    return None;
  }
  let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
  let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
  Some((width, height))
}

struct ResolvedOhosIcons {
  foreground: PathBuf,
  background: Option<PathBuf>,
  starticon: PathBuf,
}

fn resolve_ohos_icons(icons: &[String], tauri_dir: &std::path::Path) -> Option<ResolvedOhosIcons> {
  let mut foreground_path: Option<PathBuf> = None;
  let mut background_path: Option<PathBuf> = None;
  let mut starticon_path: Option<PathBuf> = None;
  let mut best_fallback: Option<(u64, PathBuf)> = None;

  for icon in icons {
    let path = PathBuf::from(icon);
    let is_png = path
      .extension()
      .and_then(|e| e.to_str())
      .map(|e| e.eq_ignore_ascii_case("png"))
      .unwrap_or(false);
    let stem_lower = path
      .file_stem()
      .and_then(|s| s.to_str())
      .map(|s| s.to_lowercase())
      .unwrap_or_default();
    let is_ohos_icon = stem_lower.ends_with("-starticon")
      || stem_lower.ends_with("-foreground")
      || stem_lower.ends_with("-background");

    if !is_png {
      if is_ohos_icon {
        log::warn!(
          "OHOS icon '{}' is not a PNG file; skipping (DevEco requires PNG)",
          path.display()
        );
      }
      continue;
    }

    let full_path = tauri_dir.join(&path);
    if !full_path.is_file() {
      continue;
    }

    if stem_lower.ends_with("-starticon") {
      starticon_path = Some(full_path);
    } else if stem_lower.ends_with("-foreground") {
      foreground_path = Some(full_path);
    } else if stem_lower.ends_with("-background") {
      background_path = Some(full_path);
    } else if let Some((w, h)) = read_png_dimensions(&full_path) {
      let area = u64::from(w) * u64::from(h);
      if best_fallback
        .as_ref()
        .map(|(best_area, _)| area > *best_area)
        .unwrap_or(true)
      {
        best_fallback = Some((area, full_path));
      }
    }
  }

  let fallback_png = best_fallback.map(|(_, p)| p).or_else(|| {
    let default_icon = tauri_dir.join("icons/icon.png");
    if default_icon.is_file() && read_png_dimensions(&default_icon).is_some() {
      Some(default_icon)
    } else {
      None
    }
  });

  if let (Some(fg), Some(bg)) = (foreground_path.clone(), background_path.clone()) {
    let starticon = starticon_path.unwrap_or_else(|| fg.clone());
    return Some(ResolvedOhosIcons {
      foreground: fg,
      background: Some(bg),
      starticon,
    });
  }

  let fg = foreground_path.or(fallback_png)?;
  let starticon = starticon_path.unwrap_or_else(|| fg.clone());
  Some(ResolvedOhosIcons {
    foreground: fg,
    background: background_path,
    starticon,
  })
}

fn write_icons_to_entries(
  project_dir: &std::path::Path,
  entries: &[&str],
  resolved: &ResolvedOhosIcons,
) -> Result<()> {
  let app_media_dir = project_dir.join("AppScope/resources/base/media");
  create_dir_all(&app_media_dir).fs_context(
    "failed to create AppScope media directory",
    app_media_dir.clone(),
  )?;
  crate::helpers::fs::copy_file(&resolved.foreground, app_media_dir.join("foreground.png"))?;
  if let Some(bg) = &resolved.background {
    crate::helpers::fs::copy_file(bg, app_media_dir.join("background.png"))?;
  } else {
    write(app_media_dir.join("background.png"), FALLBACK_BACKGROUND_PNG).fs_context(
      "failed to write fallback AppScope background icon",
      app_media_dir.join("background.png"),
    )?;
  }

  for entry in entries {
    let entry_root = project_dir.join(entry);
    if !entry_root.is_dir() {
      continue;
    }
    let entry_media_dir = entry_root.join("src/main/resources/base/media");
    create_dir_all(&entry_media_dir).fs_context(
      "failed to create entry media directory",
      entry_media_dir.clone(),
    )?;
    crate::helpers::fs::copy_file(&resolved.foreground, entry_media_dir.join("foreground.png"))?;
    if let Some(bg) = &resolved.background {
      crate::helpers::fs::copy_file(bg, entry_media_dir.join("background.png"))?;
    } else {
      write(
        entry_media_dir.join("background.png"),
        FALLBACK_BACKGROUND_PNG,
      )
      .fs_context(
        "failed to write fallback entry background icon",
        entry_media_dir.join("background.png"),
      )?;
    }
    crate::helpers::fs::copy_file(&resolved.starticon, entry_media_dir.join("startIcon.png"))?;
  }

  Ok(())
}

pub(crate) fn inject_icons(
  config: &OpenHarmonyConfig,
  tauri_config: &TauriConfig,
  tauri_dir: &std::path::Path,
) -> Result<()> {
  let Some(resolved) = resolve_ohos_icons(&tauri_config.bundle.icon, tauri_dir) else {
    log::warn!(
      "OHOS icon injection skipped: no valid PNG icons found in bundle.icon or icons/icon.png"
    );
    return Ok(());
  };

  let entry = active_entry_module();
  write_icons_to_entries(&config.project_dir(), &[entry.as_str()], &resolved)?;
  log::info!(
    "OHOS icons injected successfully (foreground={})",
    resolved.foreground.display()
  );
  Ok(())
}

pub(crate) fn inject_icons_for_all_entries(
  config: &OpenHarmonyConfig,
  tauri_config: &TauriConfig,
  tauri_dir: &std::path::Path,
) -> Result<()> {
  let Some(resolved) = resolve_ohos_icons(&tauri_config.bundle.icon, tauri_dir) else {
    log::warn!(
      "OHOS icon injection skipped during init: no valid PNG icons found in bundle.icon or icons/icon.png"
    );
    return Ok(());
  };

  write_icons_to_entries(
    &config.project_dir(),
    &["entry_mobile", "entry_desktop"],
    &resolved,
  )?;
  log::info!(
    "OHOS icons injected into all entries (foreground={})",
    resolved.foreground.display()
  );
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fake_png_with_size(w: u32, h: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(24);
    bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&w.to_be_bytes());
    bytes.extend_from_slice(&h.to_be_bytes());
    bytes
  }

  #[test]
  fn fallback_background_png_has_valid_ihdr() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("bg.png");
    std::fs::write(&p, FALLBACK_BACKGROUND_PNG).unwrap();
    assert_eq!(read_png_dimensions(&p), Some((256, 256)));
  }

  #[test]
  fn resolve_ohos_icons_prefers_explicit_layered_pair() {
    let dir = tempfile::tempdir().unwrap();
    let icons_dir = dir.path().join("icons");
    create_dir_all(&icons_dir).unwrap();
    write(icons_dir.join("128x128@2x.png"), fake_png_with_size(256, 256)).unwrap();
    write(icons_dir.join("app-foreground.png"), fake_png_with_size(1024, 1024)).unwrap();
    write(icons_dir.join("app-background.png"), fake_png_with_size(1024, 1024)).unwrap();
    write(icons_dir.join("app-starticon.png"), fake_png_with_size(144, 144)).unwrap();

    let icons = vec![
      "icons/128x128@2x.png".to_string(),
      "icons/app-foreground.png".to_string(),
      "icons/app-background.png".to_string(),
      "icons/app-starticon.png".to_string(),
    ];
    let resolved = resolve_ohos_icons(&icons, dir.path()).unwrap();
    assert_eq!(resolved.foreground, icons_dir.join("app-foreground.png"));
    assert_eq!(resolved.background, Some(icons_dir.join("app-background.png")));
    assert_eq!(resolved.starticon, icons_dir.join("app-starticon.png"));
  }

  #[test]
  fn resolve_ohos_icons_falls_back_to_largest_desktop_png() {
    let dir = tempfile::tempdir().unwrap();
    let icons_dir = dir.path().join("icons");
    create_dir_all(&icons_dir).unwrap();
    write(icons_dir.join("32x32.png"), fake_png_with_size(32, 32)).unwrap();
    write(icons_dir.join("128x128.png"), fake_png_with_size(128, 128)).unwrap();
    write(icons_dir.join("128x128@2x.png"), fake_png_with_size(256, 256)).unwrap();
    write(icons_dir.join("icon.ico"), b"not-a-png").unwrap();

    let icons = vec![
      "icons/32x32.png".to_string(),
      "icons/128x128.png".to_string(),
      "icons/128x128@2x.png".to_string(),
      "icons/icon.ico".to_string(),
    ];
    let resolved = resolve_ohos_icons(&icons, dir.path()).unwrap();
    assert_eq!(resolved.foreground, icons_dir.join("128x128@2x.png"));
    assert_eq!(resolved.background, None);
    assert_eq!(resolved.starticon, icons_dir.join("128x128@2x.png"));

    let proj = dir.path().join("gen/ohos");
    create_dir_all(proj.join("entry_desktop")).unwrap();
    create_dir_all(proj.join("entry_mobile")).unwrap();
    write_icons_to_entries(&proj, &["entry_mobile", "entry_desktop"], &resolved).unwrap();

    for path in [
      proj.join("AppScope/resources/base/media/foreground.png"),
      proj.join("entry_desktop/src/main/resources/base/media/foreground.png"),
      proj.join("entry_desktop/src/main/resources/base/media/startIcon.png"),
      proj.join("entry_mobile/src/main/resources/base/media/foreground.png"),
      proj.join("entry_mobile/src/main/resources/base/media/startIcon.png"),
    ] {
      assert_eq!(read_png_dimensions(&path), Some((256, 256)));
    }
    for bg_path in [
      proj.join("AppScope/resources/base/media/background.png"),
      proj.join("entry_desktop/src/main/resources/base/media/background.png"),
      proj.join("entry_mobile/src/main/resources/base/media/background.png"),
    ] {
      assert_eq!(std::fs::read(&bg_path).unwrap(), FALLBACK_BACKGROUND_PNG);
    }
  }
}
