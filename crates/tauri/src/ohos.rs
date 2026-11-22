// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

pub use openharmony_ability_derive;
pub use tauri_runtime::OHOSWindowKind;

/// OHOS entry-point attribute -- re-export of
/// [`tauri_macros::ohos_entry_point`](../tauri_macros/attr.ohos_entry_point.html).
///
/// Expands to the same glue as [`crate::mobile_entry_point`]; exists so the
/// two-line form names the actual platform model on every device form (the
/// Ability/NAPI entry is required on the 2in1 desktop form too, where
/// `cfg(mobile)` is false):
///
/// ```rust,ignore
/// #[cfg_attr(mobile, tauri::mobile_entry_point)]
/// #[cfg_attr(target_env = "ohos", tauri::ohos::entry_point)]
/// pub fn run() { /* ... */ }
/// ```
pub use tauri_macros::ohos_entry_point as entry_point;

/// Explicit re-export of the `openharmony-ability` types used by tauri and its macros.
pub mod openharmony_ability {
  pub use ::openharmony_ability::get_main_thread_env;
  pub use ::openharmony_ability::version;
  pub use ::openharmony_ability::OpenHarmonyApp;
}

pub static APP: Mutex<Option<openharmony_ability::OpenHarmonyApp>> = Mutex::new(None);

pub static BASE_PATH: OnceLock<Option<String>> = OnceLock::new();

pub static MODULE_NAME: OnceLock<Option<String>> = OnceLock::new();

/// Active HAP module name injected from ArkTS (`this.context.currentHapModuleInfo?.name`,
/// e.g. `entry_desktop` or `entry_mobile`). Distinct from [`MODULE_NAME`], which carries
/// the `.so` library name.
pub static HAP_MODULE_NAME: Mutex<Option<String>> = Mutex::new(None);

pub static PLUGIN_MANAGER: Mutex<Option<napi_ohos::bindgen_prelude::ObjectRef>> = Mutex::new(None);

pub struct PluginRegistration {
  pub name: String,
  pub identifier: String,
  pub class_name: String,
  pub config: serde_json::Value,
}

pub static PLUGINS_TO_REGISTER: Mutex<Vec<PluginRegistration>> = Mutex::new(Vec::new());

#[derive(Debug, Clone)]
pub struct RunCommandArgs {
  pub id: i32,
  pub plugin_name: String,
  pub command: String,
  pub payload: String,
}

pub type RunCommandTsfn =
  napi_ohos::threadsafe_function::ThreadsafeFunction<(), (), (), napi_ohos::Status, false>;

pub static RUN_COMMAND_TSFN: OnceLock<RunCommandTsfn> = OnceLock::new();

pub static RUN_COMMAND_QUEUE: Mutex<VecDeque<RunCommandArgs>> = Mutex::new(VecDeque::new());

/// Stores the active HAP module name supplied by ArkTS (`EntryAbility.onCreate`).
pub fn set_hap_module_name(name: String) {
  let trimmed = name.trim().to_string();
  if !trimmed.is_empty() {
    *HAP_MODULE_NAME.lock().unwrap_or_else(|e| e.into_inner()) = Some(trimmed);
  }
}

/// Returns the active HAP module name if it was supplied by ArkTS.
pub fn hap_module_name() -> Option<String> {
  HAP_MODULE_NAME
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .clone()
    .filter(|m| !m.is_empty())
}

fn register_plugin_if_missing<P>(
  app: &openharmony_ability::OpenHarmonyApp,
  plugin: P,
  name: &str,
) where
  P: ::openharmony_ability::BridgePlugin,
{
  match app.registered_plugin::<P>() {
    Ok(Some(_)) => {}
    Ok(None) => {
      if let Err(e) = app.register_plugin(plugin) {
        log::error!("[tauri] failed to register {name}: {e}");
      }
    }
    Err(e) => {
      log::error!("[tauri] failed to check registration for {name}: {e}");
    }
  }
}

/// Initializes the OHOS runtime singletons from the global [`APP`] instance
/// and returns the app handle.
///
/// Called once by `Builder::build` before the runtime is created. Panics when
/// the app instance is missing: the entry-point macro must run first to
/// install it.
pub fn init() -> openharmony_ability::OpenHarmonyApp {
  let ohos_app = APP.lock().unwrap().clone().expect(
    "OpenHarmony app instance not initialized -- the entry-point macro must run before Builder::build. Use #[cfg_attr(target_env = \"ohos\", tauri::ohos::entry_point)] (all device forms): on the 2in1/desktop form cfg(mobile) is always false, so a lone #[cfg_attr(mobile, tauri::mobile_entry_point)] never expands",
  );
  BASE_PATH.set(ohos_app.base_path()).ok();
  MODULE_NAME.set(ohos_app.module_name()).ok();

  // Register core Rust-side bridge plugins idempotently so multiple subsystems
  // (tauri core, tauri-runtime-wry, muda, tray-icon) never fail with duplicate
  // registration errors regardless of enabled Cargo features.
  register_plugin_if_missing(
    &ohos_app,
    openharmony_ability_plugin_window::WindowBridgePlugin,
    "WindowBridgePlugin",
  );
  register_plugin_if_missing(
    &ohos_app,
    openharmony_ability_plugin_menu::MenuBridgePlugin,
    "MenuBridgePlugin",
  );
  register_plugin_if_missing(
    &ohos_app,
    openharmony_ability_plugin_app_control::AppControlBridgePlugin,
    "AppControlBridgePlugin",
  );

  // Initialize muda's MenuClient unconditionally on OHOS (even when the
  // optional `tray-icon` feature is disabled), so application/window/context
  // menus work without tray-icon.
  match openharmony_ability_plugin_menu::MenuClient::new(&ohos_app) {
    Ok(client) => muda::set_menu_client(client),
    Err(e) => log::error!("[tauri] failed to initialize muda MenuClient: {e}"),
  }

  #[cfg(feature = "tray-icon")]
  {
    tray_icon::set_ohos_app(ohos_app.clone());
  }

  // Initialize OHOS window vibrancy WindowClient directly via openharmony-ability-plugin-window.
  crate::vibrancy::ohos::set_ohos_app(&ohos_app);

  // Initialize runtime-wry's OHOS bridge plugins for window/webview/url operations.
  #[cfg(feature = "wry")]
  tauri_runtime_wry::set_ohos_app(&ohos_app);

  ohos_app
}

pub fn dispatch_run_command(args: RunCommandArgs) {
  RUN_COMMAND_QUEUE
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .push_back(args);

  if let Some(tsfn) = RUN_COMMAND_TSFN.get() {
    tsfn.call(
      (),
      napi_ohos::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn base_path_and_module_name_accessors() {
    let _ = BASE_PATH.get();
    let _ = MODULE_NAME.get();
  }
}
