// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! The menu's own keys, live only while the menu is open.
//!
//! Layer 2 of the dispatch: above panel focus, below text entry. The menu is
//! modal but it is **not** an `InputMode`: those five variants are all text being
//! typed, and this is not text.
//!
//! Modal here means modal. Nothing falls through to [`super::global`] except the
//! quit key, so `[F]` cannot start a frequency entry behind the menu and `[W]`
//! cannot pause a waterfall you are not looking at.

use crossterm::event::{KeyCode, KeyEvent};

use crate::state::{MenuPane, MenuState};
use crate::ui::menu::{keys, sections};

use super::{global, metrics, InputCtx, KeyAction};

pub(super) fn handle(key: KeyEvent, ctx: &mut InputCtx<'_>) -> KeyAction {
    // Nothing to steer. Should not happen, since the caller only routes here
    // while the menu is open, but closing is a better answer than a panic.
    let (state, has_options) = {
        let m = metrics(ctx.state);
        (m.ui.menu, !m.device_options.is_empty())
    };
    let Some(state) = state else {
        return KeyAction::Continue;
    };

    match key.code {
        KeyCode::Esc => close(ctx),
        KeyCode::Char('q') => return KeyAction::Quit,

        KeyCode::Right if state.pane == MenuPane::Options && has_options => {
            adjust_option(ctx, state, 1)
        }
        KeyCode::Left if state.pane == MenuPane::Options && has_options => {
            adjust_option(ctx, state, -1)
        }
        KeyCode::Tab | KeyCode::Right => move_row(ctx, state, 1),
        KeyCode::BackTab | KeyCode::Left => move_row(ctx, state, -1),
        KeyCode::Down => move_down(ctx, state, 1),
        KeyCode::Up => move_down(ctx, state, -1),

        // A digit opens that slot in this section, which is the same thing the
        // digit does on the deck. The menu is a picture of the number keys, so
        // the two must not be able to disagree.
        //
        // The Keys pane has no slots, so a digit there does nothing rather than
        // acting on whichever section the cursor last sat in.
        KeyCode::Char(c @ '1'..='9') if state.pane == MenuPane::Views => {
            let slot = c as u8 - b'0';
            let target = section_of(ctx, state).and_then(|s| {
                s.entries
                    .iter()
                    .find(|e| e.slot == Some(slot))
                    .map(|e| e.preset.clone())
            });
            if let Some(name) = target {
                return open(ctx, &name);
            }
        }
        KeyCode::Enter if state.pane == MenuPane::Views => {
            let target = ctx
                .engine
                .menu()
                .at(state.section, state.entry)
                .map(|e| e.preset.clone());
            if let Some(name) = target {
                return open(ctx, &name);
            }
        }
        KeyCode::Enter if state.pane == MenuPane::Options => adjust_option(ctx, state, 1),
        _ => {}
    }
    KeyAction::Continue
}

/// Load a layout and close the menu.
fn open(ctx: &mut InputCtx<'_>, preset: &str) -> KeyAction {
    let action = global::presets::try_set_preset(ctx.engine, ctx.state, preset);
    close(ctx);
    action
}

fn close(ctx: &mut InputCtx<'_>) {
    metrics(ctx.state).ui.menu = None;
}

fn section_of<'a>(
    ctx: &'a InputCtx<'_>,
    state: MenuState,
) -> Option<&'a crate::ui::menu::model::Section> {
    let (si, _) = ctx.engine.menu().clamp(state.section, state.entry)?;
    ctx.engine.menu().sections.get(si)
}

/// Step down the left column: the sections, then the panes under the rule.
///
/// One index over both kinds of row, so `Tab` walks the column exactly as it is
/// drawn and cannot skip the rule or land on it.
fn move_row(ctx: &mut InputCtx<'_>, state: MenuState, step: isize) {
    let menu = ctx.engine.menu();
    let rows = sections::row_count(menu);
    if rows == 0 {
        return;
    }
    let Some((si, _)) = menu.clamp(state.section, state.entry) else {
        return;
    };
    let next = wrap(sections::selected_row(menu, si, state.pane), step, rows);

    let updated = match sections::row_target(menu, next) {
        Ok(section) => {
            // The new section may be shorter than the one we came from.
            let len = menu.sections[section].entries.len();
            MenuState {
                section,
                entry: state.entry.min(len.saturating_sub(1)),
                pane: MenuPane::Views,
                scroll: 0,
            }
        }
        // Leaving for a pane keeps `section` and `entry`, so coming back lands
        // where you were rather than at the top.
        Err(pane) => MenuState {
            pane,
            scroll: 0,
            ..state
        },
    };
    metrics(ctx.state).ui.menu = Some(updated);
}

/// Up and down: through a section's layouts, or through the key reference.
fn move_down(ctx: &mut InputCtx<'_>, state: MenuState, step: isize) {
    match state.pane {
        MenuPane::Views => {
            let Some((si, ei)) = ctx.engine.menu().clamp(state.section, state.entry) else {
                return;
            };
            let count = ctx.engine.menu().sections[si].entries.len();
            if count == 0 {
                return;
            }
            metrics(ctx.state).ui.menu = Some(MenuState {
                section: si,
                entry: wrap(ei, step, count),
                ..state
            });
        }
        // The reference is taller than a short terminal, so it scrolls. It does
        // not wrap: a list you are reading top to bottom should stop at the
        // bottom rather than silently start again.
        MenuPane::Keys => {
            let last = {
                let m = metrics(ctx.state);
                keys::row_count_for(&m.caps).saturating_sub(1)
            };
            let next = if step >= 0 {
                (state.scroll + 1).min(last)
            } else {
                state.scroll.saturating_sub(1)
            };
            metrics(ctx.state).ui.menu = Some(MenuState {
                scroll: next,
                ..state
            });
        }
        MenuPane::Options => {
            let count = metrics(ctx.state).device_options.len();
            if count == 0 {
                return;
            }
            metrics(ctx.state).ui.menu = Some(MenuState {
                scroll: wrap(state.scroll.min(count - 1), step, count),
                ..state
            });
        }
    }
}

fn adjust_option(ctx: &mut InputCtx<'_>, state: MenuState, step: isize) {
    let Some(device) = ctx.device else {
        return;
    };
    apply_option_change(ctx.state, state.scroll, step, |id, choice| {
        device.set_option(id, choice)
    });
}

fn apply_option_change(
    state: &std::sync::Arc<std::sync::Mutex<crate::state::SdrMetrics>>,
    option_index: usize,
    step: isize,
    apply: impl FnOnce(&str, &str) -> anyhow::Result<()>,
) {
    let requested = {
        let m = metrics(state);
        let Some(option) = m.device_options.get(option_index) else {
            return;
        };
        if option.choices.is_empty() {
            return;
        }
        let current = option
            .choices
            .iter()
            .position(|choice| choice == &option.selected_choice);
        let selected = match current {
            Some(index) => wrap(index, step, option.choices.len()),
            None if step >= 0 => 0,
            None => option.choices.len() - 1,
        };
        (
            option.id.clone(),
            option.label.clone(),
            option.choices[selected].clone(),
        )
    };

    let result = apply(&requested.0, &requested.2);
    let mut m = metrics(state);
    match result {
        Ok(()) => {
            if let Some(option) = m
                .device_options
                .iter_mut()
                .find(|option| option.id == requested.0)
            {
                option.selected_choice.clone_from(&requested.2);
            }
            m.push_log(format!("{} set to {}", requested.1, requested.2));
        }
        Err(error) => m.push_log(format!("{} error: {error}", requested.1)),
    }
}

/// `i + step` modulo `len`, for a step of -1 or 1. Pure, so the wrap-around at
/// both ends is testable without a terminal.
fn wrap(i: usize, step: isize, len: usize) -> usize {
    debug_assert!(len > 0);
    if step >= 0 {
        (i + step as usize) % len
    } else {
        (i + len - (step.unsigned_abs() % len)) % len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LayoutConfig;
    use crate::hardware::DeviceOption;
    use crate::state::SdrMetrics;
    use crate::ui::{self, PanelRegistry};
    use crossterm::event::KeyEvent;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Drives the menu layer the way the app does, minus the terminal. Same
    /// shape as the harness in `global/mod.rs`, and deliberately device-free:
    /// nothing the menu does should reach the radio.
    struct Harness {
        state: Arc<Mutex<SdrMetrics>>,
        engine: ui::LayoutEngine,
        show_footer: bool,
        focus_keys: HashMap<char, &'static str>,
    }

    impl Harness {
        fn new() -> Self {
            let mut engine =
                ui::LayoutEngine::new(LayoutConfig::default_config(), PanelRegistry::new());
            engine.set_preset("command_rail");
            let state = Arc::new(Mutex::new(SdrMetrics::fixture()));
            state.lock().unwrap().ui.menu = Some(MenuState::default());
            Self {
                state,
                engine,
                show_footer: true,
                focus_keys: HashMap::new(),
            }
        }

        fn key(&mut self, code: KeyCode) -> KeyAction {
            let mut ctx = InputCtx {
                state: &self.state,
                device: None,
                engine: &mut self.engine,
                show_footer: &mut self.show_footer,
                focus_keys: &self.focus_keys,
            };
            handle(KeyEvent::from(code), &mut ctx)
        }

        fn menu(&self) -> MenuState {
            self.state.lock().unwrap().ui.menu.expect("menu is open")
        }

        fn show_options(&mut self, options: Vec<DeviceOption>) {
            let mut m = self.state.lock().unwrap();
            m.device_options = options;
            m.ui.menu = Some(MenuState {
                pane: MenuPane::Options,
                ..MenuState::default()
            });
        }
    }

    fn option(id: &str, label: &str, selected: &str) -> DeviceOption {
        DeviceOption {
            id: id.into(),
            label: label.into(),
            choices: vec!["Narrow".into(), "Wide".into()],
            selected_choice: selected.into(),
        }
    }

    /// Tab walks the column exactly as it is drawn: every section, then Keys,
    /// then Options, then round to the top. The panes are the two rows under the
    /// rule, so this is what proves Options is reachable at all.
    #[test]
    fn tab_walks_the_sections_then_both_panes_then_wraps() {
        let mut h = Harness::new();
        let sections = h.engine.menu().sections.len();
        for _ in 0..sections - 1 {
            h.key(KeyCode::Tab);
        }
        assert_eq!(h.menu().pane, MenuPane::Views, "still on the last section");

        h.key(KeyCode::Tab);
        assert_eq!(h.menu().pane, MenuPane::Keys);
        h.key(KeyCode::Tab);
        assert_eq!(h.menu().pane, MenuPane::Options);
        h.key(KeyCode::Tab);
        assert_eq!(h.menu().pane, MenuPane::Views, "wraps back to the sections");
        assert_eq!(h.menu().section, 0);
    }

    /// Options holds nothing yet, so the keys that act on a list are quiet
    /// rather than acting on whichever section the cursor came from. A digit
    /// there must not load a layout behind the reader's back.
    #[test]
    fn options_has_nothing_to_steer() {
        let mut h = Harness::new();
        h.state.lock().unwrap().ui.menu = Some(MenuState {
            section: 1,
            entry: 2,
            pane: MenuPane::Options,
            scroll: 0,
        });
        let before = h.menu();

        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Char('1'),
            KeyCode::Enter,
        ] {
            h.key(code);
            assert_eq!(h.menu(), before, "{code:?} moved something");
        }
        assert_eq!(
            h.engine.active_preset(),
            "command_rail",
            "no key in Options may load a layout"
        );
    }

    #[test]
    fn options_move_up_and_down_through_device_settings() {
        let mut h = Harness::new();
        h.show_options(vec![
            option("bandwidth", "Bandwidth", "Narrow"),
            option("mode", "Mode", "Wide"),
        ]);

        h.key(KeyCode::Down);
        assert_eq!(h.menu().scroll, 1);
        h.key(KeyCode::Down);
        assert_eq!(h.menu().scroll, 0);
        h.key(KeyCode::Up);
        assert_eq!(h.menu().scroll, 1);
    }

    #[test]
    fn horizontal_value_keys_do_not_leave_options_when_values_exist() {
        let mut h = Harness::new();
        h.show_options(vec![option("bandwidth", "Bandwidth", "Narrow")]);
        let before = h.menu();

        h.key(KeyCode::Right);

        assert_eq!(h.menu(), before);
    }

    #[test]
    fn horizontal_keys_keep_the_old_navigation_for_empty_devices() {
        let mut h = Harness::new();
        h.state.lock().unwrap().ui.menu = Some(MenuState {
            pane: MenuPane::Options,
            ..MenuState::default()
        });

        h.key(KeyCode::Left);
        assert_eq!(h.menu().pane, MenuPane::Keys);

        h.state.lock().unwrap().ui.menu = Some(MenuState {
            pane: MenuPane::Options,
            ..MenuState::default()
        });
        h.key(KeyCode::Right);
        assert_eq!(h.menu().pane, MenuPane::Views);
        assert_eq!(h.menu().section, 0);
    }

    #[test]
    fn a_successful_change_updates_state_after_the_device_call() {
        let state = Arc::new(Mutex::new(SdrMetrics::fixture()));
        state
            .lock()
            .unwrap()
            .device_options
            .push(option("bandwidth", "Bandwidth", "Narrow"));
        let mut call = None;

        apply_option_change(&state, 0, 1, |id, choice| {
            assert!(
                state.try_lock().is_ok(),
                "state was locked during device call"
            );
            call = Some((id.to_string(), choice.to_string()));
            Ok(())
        });

        let m = state.lock().unwrap();
        assert_eq!(call, Some(("bandwidth".to_string(), "Wide".to_string())));
        assert_eq!(m.device_options[0].selected_choice, "Wide");
        assert!(m
            .ui
            .log
            .back()
            .is_some_and(|entry| entry.text.contains("Bandwidth set to Wide")));
    }

    #[test]
    fn a_failed_change_keeps_state_and_surfaces_the_error() {
        let state = Arc::new(Mutex::new(SdrMetrics::fixture()));
        state
            .lock()
            .unwrap()
            .device_options
            .push(option("bandwidth", "Bandwidth", "Narrow"));

        apply_option_change(&state, 0, 1, |_, _| {
            assert!(
                state.try_lock().is_ok(),
                "state was locked during device call"
            );
            anyhow::bail!("device rejected choice")
        });

        let m = state.lock().unwrap();
        assert_eq!(m.device_options[0].selected_choice, "Narrow");
        assert!(m.ui.log.back().is_some_and(|entry| entry
            .text
            .contains("Bandwidth error: device rejected choice")));
    }

    /// A pane is a detour, not a reset: the place you had in the list survives
    /// the visit, so stepping back onto the sections does not start you at the
    /// top of one.
    #[test]
    fn a_pane_visit_does_not_disturb_the_place_in_the_list() {
        let mut h = Harness::new();
        // The last section, because that is the one the panes are entered from.
        // Walking there first would move the cursor on its own and prove nothing
        // about the panes.
        let last = h.engine.menu().sections.len() - 1;
        let place = MenuState {
            section: last,
            entry: 1,
            pane: MenuPane::Views,
            scroll: 0,
        };
        h.state.lock().unwrap().ui.menu = Some(place);

        h.key(KeyCode::Tab);
        assert_eq!(h.menu().pane, MenuPane::Keys);
        assert_eq!((h.menu().section, h.menu().entry), (last, 1));
        h.key(KeyCode::Tab);
        assert_eq!(h.menu().pane, MenuPane::Options);
        assert_eq!((h.menu().section, h.menu().entry), (last, 1));
    }

    #[test]
    fn wrap_moves_forward_and_back() {
        assert_eq!(wrap(0, 1, 4), 1);
        assert_eq!(wrap(3, 1, 4), 0, "forward off the end comes round");
        assert_eq!(wrap(1, -1, 4), 0);
        assert_eq!(wrap(0, -1, 4), 3, "back off the front comes round");
    }

    #[test]
    fn wrap_handles_a_single_entry() {
        assert_eq!(wrap(0, 1, 1), 0);
        assert_eq!(wrap(0, -1, 1), 0);
    }
}
