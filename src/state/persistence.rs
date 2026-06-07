//! State file persistence under `$XDG_RUNTIME_DIR/driftwm/state`.
//!
//! External tools (launcher, status bars, scripts) read this file to learn
//! the current camera/zoom and window/layer inventory. Writes are throttled
//! to ~10/sec and only fire when something actually changed.
//!
//! The `windows=` line is a JSON array of objects with fields `app_id`,
//! `title`, `position` ([x, y]), `size` ([w, h]), and booleans
//! `is_focused`/`is_widget`. Position/size match the window-rules format in
//! config.toml: position is the **window center** with **Y-up** convention.

use smithay::utils::{Logical, Point};
use smithay::wayland::seat::WaylandFocus;
use std::collections::HashMap;
use std::time::Instant;

use driftwm::window_ext::WindowExt;

use crate::ipc::protocol::WindowInfo;

use super::{DriftWm, output_state};

// Title is intentionally excluded from change detection: apps update their title
// on every keystroke / tab switch, and a write per title change would spam
// consumers. Title is still serialized when some other change triggers a write.
fn window_list_changed(a: &[WindowInfo], b: &[WindowInfo]) -> bool {
    a.len() != b.len()
        || a.iter().zip(b).any(|(x, y)| {
            x.app_id != y.app_id
                || x.position != y.position
                || x.size != y.size
                || x.is_focused != y.is_focused
                || x.is_widget != y.is_widget
        })
}

impl DriftWm {
    /// The canvas window inventory in the shared [`WindowInfo`] shape (position =
    /// window center, Y-up), focused window first. Single source of truth for
    /// both the state file and the IPC `state` response, so the two can't drift.
    pub fn window_inventory(&self) -> Vec<WindowInfo> {
        let focused = self.focused_window();
        let mut windows: Vec<WindowInfo> = Vec::new();
        for window in self.space.elements() {
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            // Pinned windows have no canvas position; omit from the inventory
            // (state file + IPC `state`), whose `position` is a canvas coord.
            if self.is_pinned(window) {
                continue;
            }
            let (app_id, title) = smithay::wayland::compositor::with_states(&surface, |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok())
                    .map(|g| {
                        (
                            g.app_id.clone().unwrap_or_default(),
                            g.title.clone().unwrap_or_default(),
                        )
                    })
                    .unwrap_or_default()
            });
            if app_id.is_empty() {
                continue;
            }
            let loc = self.space.element_location(window).unwrap_or_default();
            // window.geometry().size can flicker for some Chromium-class clients
            // (see fit.rs), causing the occasional spurious write.
            let size = window.geometry().size;
            let (rx, ry) = driftwm::canvas::internal_to_rule(loc, size);
            windows.push(WindowInfo {
                app_id,
                title,
                position: [rx, ry],
                size: [size.w, size.h],
                is_focused: focused.as_ref() == Some(window),
                is_widget: window.is_widget(),
            });
        }
        // Focused window first, so consumers can read windows[0] as the focused one.
        if let Some(idx) = windows.iter().position(|w| w.is_focused) {
            let w = windows.remove(idx);
            windows.insert(0, w);
        }
        windows
    }

    /// Write viewport center + zoom to `$XDG_RUNTIME_DIR/driftwm/state` if changed.
    /// Atomic: writes to .tmp then renames.
    pub fn write_state_file_if_dirty(&mut self) {
        let window_fps = self.window_inventory();

        // Layer-shell namespaces (waybar, notifications, etc.).
        let mut layers: Vec<String> = Vec::new();
        for output in self.space.outputs() {
            let layer_map = smithay::desktop::layer_map_for_output(output);
            for layer in layer_map.layers() {
                let ns = layer.namespace().to_string();
                if !ns.is_empty() && !layers.contains(&ns) {
                    layers.push(ns);
                }
            }
        }

        let layout_dirty = self.state_file_layout != self.active_layout;
        let mut any_output_dirty = false;
        for output in self.space.outputs() {
            let os = output_state(output);
            let name = output.name();
            let (cam, z) = (os.camera, os.zoom);
            drop(os);
            if let Some(&(cached_cam, cached_z)) = self.state_file_cameras.get(&name) {
                if (cam.x - cached_cam.x).abs() >= 0.5
                    || (cam.y - cached_cam.y).abs() >= 0.5
                    || (z - cached_z).abs() >= 0.001
                {
                    any_output_dirty = true;
                    break;
                }
            } else {
                any_output_dirty = true;
                break;
            }
        }
        let windows_dirty = window_list_changed(&window_fps, &self.state_file_windows)
            || layers.len() != self.state_file_layer_count;

        if !layout_dirty && !any_output_dirty && !windows_dirty {
            return;
        }
        // Throttle attempts to ~10/sec max (100ms between writes). Updated even
        // if the write below fails — we still want to limit retry frequency.
        if self.state_file_last_write.elapsed() < std::time::Duration::from_millis(100) {
            return;
        }
        self.state_file_last_write = Instant::now();

        let z = self.zoom();
        let vp = self.get_viewport_size();
        let (cx, cy) = driftwm::canvas::viewport_center(self.camera(), z, vp);

        let Some(dir) = state_file_dir() else { return };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("state");
        let tmp = dir.join("state.tmp");
        let mut content = format!(
            "x={cx:.0}\ny={cy:.0}\nzoom={z:.3}\nlayout={}\n",
            self.active_layout
        );

        if let Some(output) = self.active_output() {
            let home_return = output_state(&output).home_return.clone();
            if let Some(ref ret) = home_return {
                let sz = ret.zoom;
                let (sx, sy) = driftwm::canvas::viewport_center(ret.camera, sz, vp);
                content += &format!("saved_x={sx:.0}\nsaved_y={sy:.0}\nsaved_zoom={sz:.3}\n");
            }
        }

        if !window_fps.is_empty()
            && let Ok(json) = serde_json::to_string(&window_fps)
        {
            content += "windows=";
            content += &json;
            content.push('\n');
        }

        if !layers.is_empty() {
            content += &format!("layers={}\n", layers.join(","));
        }

        // Per-output camera/zoom state
        for output in self.space.outputs() {
            let os = output_state(output);
            let name = output.name();
            content += &format!(
                "outputs.{name}.camera_x={:.1}\noutputs.{name}.camera_y={:.1}\noutputs.{name}.zoom={:.3}\n",
                os.camera.x, os.camera.y, os.zoom
            );
        }

        // Update content caches only after a successful atomic rename, so a
        // transient FS error gets retried on the next call instead of being
        // silently swallowed.
        if std::fs::write(&tmp, content).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
            self.state_file_layer_count = layers.len();
            for output in self.space.outputs() {
                let os = output_state(output);
                self.state_file_cameras
                    .insert(output.name(), (os.camera, os.zoom));
            }
            self.state_file_layout = self.active_layout.clone();
            self.state_file_windows = window_fps;
        }
    }
}

fn state_file_dir() -> Option<std::path::PathBuf> {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(|d| std::path::PathBuf::from(d).join("driftwm"))
}

/// Remove the state file on compositor exit.
pub fn remove_state_file() {
    if let Some(dir) = state_file_dir() {
        let _ = std::fs::remove_file(dir.join("state"));
        let _ = std::fs::remove_file(dir.join("state.tmp"));
    }
}

/// Read all per-output camera/zoom entries from the state file.
/// Returns a map from output name to `(camera, zoom)`.
pub fn read_all_per_output_state() -> HashMap<String, (Point<f64, Logical>, f64)> {
    let mut result = HashMap::new();
    let Some(dir) = state_file_dir() else {
        return result;
    };
    let Ok(content) = std::fs::read_to_string(dir.join("state")) else {
        return result;
    };

    // Parse lines like "outputs.eDP-1.camera_x=123.4"
    type Partial = (Option<f64>, Option<f64>, Option<f64>);
    let mut entries: HashMap<String, Partial> = HashMap::new();
    for line in content.lines() {
        let Some(rest) = line.strip_prefix("outputs.") else {
            continue;
        };
        // rest = "eDP-1.camera_x=123.4"
        let Some((name_and_key, val_str)) = rest.split_once('=') else {
            continue;
        };
        let Ok(val) = val_str.parse::<f64>() else {
            continue;
        };
        if let Some(name) = name_and_key.strip_suffix(".camera_x") {
            entries.entry(name.to_string()).or_default().0 = Some(val);
        } else if let Some(name) = name_and_key.strip_suffix(".camera_y") {
            entries.entry(name.to_string()).or_default().1 = Some(val);
        } else if let Some(name) = name_and_key.strip_suffix(".zoom") {
            entries.entry(name.to_string()).or_default().2 = Some(val);
        }
    }
    for (name, (cx, cy, z)) in entries {
        if let (Some(x), Some(y), Some(zoom)) = (cx, cy, z) {
            result.insert(name, (Point::from((x, y)), zoom));
        }
    }
    result
}
