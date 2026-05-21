//! Config hot-reload. On parse failure the old config is kept and an error
//! is logged — a bad edit never crashes the compositor.

use smithay::input::keyboard::XkbConfig;

use super::{DriftWm, output_state};

impl DriftWm {
    /// Hot-reload config from disk. On parse failure, logs an error and keeps the old config.
    pub fn reload_config(&mut self) {
        let config_path = driftwm::config::config_path();
        let contents = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    "Config reload: failed to read {}: {e}",
                    config_path.display()
                );
                return;
            }
        };
        let mut new_config = match driftwm::config::Config::from_toml(&contents) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Config reload: parse error: {e}");
                return;
            }
        };

        // Hot-reload keyboard layout
        if new_config.keyboard_layout != self.config.keyboard_layout {
            let kb = &new_config.keyboard_layout;
            let xkb = XkbConfig {
                layout: &kb.layout,
                variant: &kb.variant,
                options: if kb.options.is_empty() {
                    None
                } else {
                    Some(kb.options.clone())
                },
                model: &kb.model,
                ..Default::default()
            };
            let keyboard = self.seat.get_keyboard().unwrap();
            let num_lock = keyboard.modifier_state().num_lock;
            if let Err(err) = keyboard.set_xkb_config(self, xkb) {
                tracing::warn!("Config reload: error updating keyboard layout: {err:?}");
                new_config.keyboard_layout = self.config.keyboard_layout.clone();
            } else {
                tracing::info!("Config reload: keyboard layout updated");
                let mut mods = keyboard.modifier_state();
                if mods.num_lock != num_lock {
                    mods.num_lock = num_lock;
                    keyboard.set_modifier_state(mods);
                }
            }
        }
        if new_config.autostart != self.config.autostart {
            tracing::info!("Config reload: autostart changes only apply at startup");
        }

        // Keyboard repeat rate/delay
        if new_config.repeat_rate != self.config.repeat_rate
            || new_config.repeat_delay != self.config.repeat_delay
        {
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.change_repeat_info(new_config.repeat_rate, new_config.repeat_delay);
        }

        // Momentum friction — apply to all outputs
        if new_config.friction != self.config.friction {
            for output in self.space.outputs() {
                output_state(output).momentum.friction = new_config.friction;
            }
        }

        // Background source — always clear cached state so that editing
        // the shader file on disk takes effect after `touch`ing the config,
        // and switching `type` between shader/tile/wallpaper swaps modes cleanly.
        // Reset `background_is_animated`: stale `true` (left over from a
        // previous animated shader) defeats wallpaper/tile damage savings by
        // forcing per-frame redraws.
        self.render.background_shader = None;
        self.render.background_is_animated = false;
        self.render.cached_bg_elements.clear();
        self.render.tile_shader = None;
        self.render.cached_tile_bg.clear();
        self.render.wallpaper_shader = None;
        self.render.cached_wallpaper_bg.clear();

        // Per-window border + shadow elements bake `corner_radius` and the
        // border color into uniforms refreshed only when the phys-key changes
        // — neither key carries color or corner radius. Drop both caches so a
        // user edit to `decorations.border_color*` or `decorations.corner_radius`
        // is picked up on the next frame.
        self.render.border_cache.clear();
        self.render.shadow_cache.clear();

        // Cursor theme/size — validate theme before committing. Env vars stay
        // out of process env: child_env (rebuilt by `Config::from_raw`) carries
        // XCURSOR_* to spawned children, and the cursor loader reads from
        // `self.config` directly.
        let theme_changed = new_config.cursor_theme != self.config.cursor_theme;
        let size_changed = new_config.cursor_size != self.config.cursor_size;
        if theme_changed || size_changed {
            let theme_ok = if theme_changed {
                if let Some(ref theme_name) = new_config.cursor_theme {
                    let theme = xcursor::CursorTheme::load(theme_name);
                    if theme.load_icon("default").is_some() {
                        true
                    } else {
                        tracing::warn!(
                            "Cursor theme '{theme_name}' not found, keeping current theme"
                        );
                        new_config.cursor_theme = self.config.cursor_theme.clone();
                        match &self.config.cursor_theme {
                            Some(t) => {
                                new_config.child_env.insert("XCURSOR_THEME".into(), t.clone());
                            }
                            None => {
                                new_config.child_env.remove("XCURSOR_THEME");
                            }
                        }
                        false
                    }
                } else {
                    true
                }
            } else {
                false
            };

            if theme_ok || size_changed {
                self.cursor.cursor_buffers.clear();
            }
        }

        // Trackpad settings — reconfigure all connected devices
        if new_config.trackpad != self.config.trackpad {
            self.config.trackpad = new_config.trackpad.clone();
            let devices = self.input_devices.clone();
            for mut device in devices {
                self.configure_libinput_device(&mut device);
            }
            tracing::info!("Config reload: trackpad settings applied to all devices");
        }

        // Env vars: child_env is rebuilt by `Config::from_raw`, so spawned
        // children pick up new values automatically. Process env stays
        // untouched. DISPLAY (set by xwayland::setup at startup) is kept by
        // copying it forward when a satellite is running.
        if let Some(display) = self.config.child_env.get("DISPLAY").cloned() {
            new_config.child_env.insert("DISPLAY".into(), display);
        }

        self.config = new_config;

        // Force every SSD title bar to re-render with the new decoration config
        // (font, height, colors). `update()` only rebuilds the buffer when its
        // cached width/focus/scale differ, so invalidate the cached width.
        for deco in self.decorations.values_mut() {
            deco.width = -1;
        }

        self.apply_output_rules_after_reload();
        self.recompute_decoration_scale();

        self.mark_all_dirty();
        tracing::info!("Config reloaded");
    }

    /// Re-apply per-output rules (mode, scale, transform, position) to existing
    /// outputs. Mode changes go through `pending_mode_changes` and are picked
    /// up by the udev backend's render loop; everything else applies in-place
    /// via `Output::change_current_state`. Uses the same lookup path as
    /// `create_surface` (`Config::output_config`) so reload and startup compute
    /// state identically.
    fn apply_output_rules_after_reload(&mut self) {
        use driftwm::config::{OutputMode as ConfigOutputMode, OutputPosition};
        use smithay::utils::Transform;

        let outputs: Vec<smithay::output::Output> = self.space.outputs().cloned().collect();
        // Track cumulative width for auto-positioning, mirroring `create_surface`'s
        // algorithm in udev.rs. Outputs processed earlier in iteration order get
        // smaller x; widths are read post-change_current_state so a scale-change
        // affects subsequent outputs' auto positions correctly.
        let mut auto_x: i32 = 0;
        for output in outputs {
            let name = output.name();
            let cfg = self.config.output_config(&name);

            let want_mode = cfg.map(|c| &c.mode).cloned().unwrap_or_default();
            if let Some(current) = output.current_mode() {
                let (cur_w, cur_h) = (current.size.w, current.size.h);
                let cur_hz_milli = current.refresh;
                let intent = match &want_mode {
                    ConfigOutputMode::Size(w, h) if (cur_w, cur_h) != (*w, *h) => {
                        Some(crate::state::ModeIntent::Custom {
                            w: *w,
                            h: *h,
                            refresh_mhz: cur_hz_milli,
                        })
                    }
                    ConfigOutputMode::SizeRefresh(w, h, hz)
                        if (cur_w, cur_h) != (*w, *h) || cur_hz_milli != *hz as i32 * 1000 =>
                    {
                        Some(crate::state::ModeIntent::Custom {
                            w: *w,
                            h: *h,
                            refresh_mhz: *hz as i32 * 1000,
                        })
                    }
                    ConfigOutputMode::Preferred => {
                        // We don't currently re-modeset when a rule reverts
                        // to "preferred" — log so the user understands why
                        // their old custom mode is still active.
                        tracing::info!(
                            "Config reload: output '{name}' rule is 'preferred', \
                             but reverting from a custom mode isn't supported live — \
                             restart driftwm or replug to apply"
                        );
                        None
                    }
                    _ => None,
                };
                if let Some(intent) = intent {
                    self.pending_mode_changes.insert(
                        name.clone(),
                        crate::state::PendingMode {
                            intent,
                            retry_count: 0,
                        },
                    );
                }
            }

            // Scale: missing field = revert to 1.0 (default).
            let want_scale = cfg.and_then(|c| c.scale).unwrap_or(1.0);
            let cur_scale = output.current_scale().fractional_scale();
            let new_scale = if (cur_scale - want_scale).abs() > f64::EPSILON {
                Some(smithay::output::Scale::Fractional(want_scale))
            } else {
                None
            };

            // Transform: missing field = revert to Normal.
            let want_transform = cfg.and_then(|c| c.transform).unwrap_or(Transform::Normal);
            let new_transform = if output.current_transform() != want_transform {
                Some(want_transform)
            } else {
                None
            };

            // Position: missing field (or `Auto`) = auto-place by accumulated
            // width, mirroring `create_surface` in udev.rs.
            let want_position: smithay::utils::Point<i32, smithay::utils::Logical> =
                match cfg.map(|c| &c.position) {
                    Some(OutputPosition::Fixed(x, y)) => (*x, *y).into(),
                    _ => (auto_x, 0).into(),
                };
            let cur_position = crate::state::output_state(&output).layout_position;
            let new_position = if cur_position != want_position {
                let mut os = crate::state::output_state(&output);
                os.layout_position = want_position;
                Some(want_position)
            } else {
                None
            };

            if new_scale.is_some() || new_transform.is_some() || new_position.is_some() {
                output.change_current_state(None, new_transform, new_scale, new_position);
                {
                    let mut map = smithay::desktop::layer_map_for_output(&output);
                    map.arrange();
                }
                let size = crate::state::output_logical_size(&output);
                self.resize_fullscreen_for_output(&output, size);
                self.render.remove_output(&name);
                self.output_config_dirty = true;
            }

            // Advance auto_x using the output's *post-change* logical width so
            // a scale change on one output affects the next output's auto x.
            auto_x += crate::state::output_logical_size(&output).w;
        }
    }
}
