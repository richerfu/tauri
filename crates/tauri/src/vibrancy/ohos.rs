// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::OnceLock;

use openharmony_ability::OpenHarmonyApp;
use openharmony_ability_plugin_window::WindowClient;
use tauri_runtime::WindowDispatch;

use crate::utils::config::{Color, WindowEffectsConfig};
use crate::window::Effect;
use crate::{Runtime, Window};

static WINDOW_CLIENT: OnceLock<WindowClient> = OnceLock::new();

pub(crate) fn set_ohos_app(app: &OpenHarmonyApp) {
  match WindowClient::new(app) {
    Ok(client) => {
      let _ = WINDOW_CLIENT.set(client);
    }
    Err(e) => {
      log::error!("[vibrancy] failed to create WindowClient: {e}");
    }
  }
}

fn argb_from_color(Color(r, g, b, a): Color) -> u32 {
  ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

fn spawn_vibrancy_task<F>(description: &'static str, fut: F)
where
  F: std::future::Future<Output = napi_ohos::Result<()>> + Send + 'static,
{
  std::thread::spawn(move || {
    if let Err(e) = futures_executor::block_on(fut) {
      log::error!("[vibrancy] {description} failed: {e}");
    }
  });
}

pub fn apply_effects<R: Runtime>(window: &Window<R>, effects: WindowEffectsConfig) {
  let WindowEffectsConfig {
    effects,
    radius,
    color,
    ..
  } = effects;

  let window_id = match window.window.dispatcher.ohos_window_id() {
    Ok(Some(id)) => id,
    Ok(None) => {
      log::warn!(
        "[vibrancy] ohos_window_id returned None — tao window_id not set; skipping effects"
      );
      return;
    }
    Err(e) => {
      log::error!("[vibrancy] ohos_window_id failed: {:?}", e);
      return;
    }
  };

  let Some(effect) = effects.into_iter().next() else {
    return;
  };

  let blur_radius = radius.unwrap_or(20.0);

  match effect {
    // Mica/Tabbed series intentionally unsupported on OHOS — skip cleanly.
    Effect::Mica
    | Effect::MicaDark
    | Effect::MicaLight
    | Effect::Tabbed
    | Effect::TabbedDark
    | Effect::TabbedLight => {
      log::info!("[vibrancy] Mica/Tabbed effects are not supported on OHOS; skipping");
    }
    Effect::Acrylic => {
      let Some(client) = WINDOW_CLIENT.get().cloned() else {
        log::warn!("[vibrancy] WindowClient not initialized; skipping Acrylic effect");
        return;
      };
      let argb = color.map(argb_from_color);
      spawn_vibrancy_task("apply Acrylic", async move {
        client.set_window_blur(window_id, blur_radius).await?;
        if let Some(argb) = argb {
          client.set_window_background_color(window_id, argb).await?;
        }
        Ok(())
      });
      log::info!(
        "[vibrancy] scheduled Acrylic effect (radius={blur_radius}) for window_id {window_id}"
      );
    }
    // Effect::Blur and macOS-specific effects: approximate via window blur.
    _ => {
      let Some(client) = WINDOW_CLIENT.get().cloned() else {
        log::warn!("[vibrancy] WindowClient not initialized; skipping Blur effect");
        return;
      };
      spawn_vibrancy_task("apply Blur", async move {
        client.set_window_blur(window_id, blur_radius).await
      });
      log::info!(
        "[vibrancy] scheduled effect {effect:?} (radius={blur_radius}) for window_id {window_id}"
      );
    }
  }
}

pub fn clear_effects<R: Runtime>(window: &Window<R>) {
  if let Ok(Some(window_id)) = window.window.dispatcher.ohos_window_id() {
    if let Some(client) = WINDOW_CLIENT.get().cloned() {
      spawn_vibrancy_task("clear blur", async move {
        client.set_window_blur(window_id, 0.0).await
      });
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn argb_packs_rgba_correctly() {
    assert_eq!(argb_from_color(Color(0x11, 0x22, 0x33, 0x44)), 0x4411_2233);
    assert_eq!(argb_from_color(Color(0xFF, 0x00, 0x80, 0xCC)), 0xCCFF_0080);
  }
}
