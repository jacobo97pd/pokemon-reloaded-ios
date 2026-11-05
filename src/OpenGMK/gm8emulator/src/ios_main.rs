//! Glue between the iOS app (`ios/host/main.m`) and the engine.
//!
//! The counterpart of `android_main.rs`. The engine reaches it as `crate::android_main` (see
//! `lib.rs`), so every function the engine calls there exists here with the same signature.
//!
//! The app owns UIKit: the window, the GL layer, the keyboard, the audio session. It calls the
//! `rl_ios_*` functions below on the main thread for touches, typed text, size changes, the
//! foreground/background switch and a per-frame tick. The engine runs on a worker thread of its
//! own, exactly as on Android, and reaches back to the app through the `rl_host_*` functions.
//!
//! The touch handling is a port of Android's: on-screen buttons press the keys they map to,
//! everything else reaches the game as a mouse, with the same timing tricks so menus react to a
//! finger the way they do to a click.

use std::{
    collections::{HashMap, HashSet},
    ffi::{c_char, c_void, CStr, CString},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use ramen::{
    input::Key,
    platform::android::{self as platform, InputEvent},
};

use crate::touch_controls as tc;

// ---------------------------------------------------------------------------------------------
// The app's side, implemented in Objective-C.

extern "C" {
    /// Raises or drops the system keyboard.
    fn rl_host_set_keyboard(visible: bool);
    /// A light tap of the haptic engine, for the on-screen buttons.
    fn rl_host_haptic();
    /// Shows `text` over the game (an empty string hides it). Used for setup steps and errors,
    /// which is the only way the player can see why the game did not start.
    fn rl_host_show_message(text: *const c_char);
    /// The game ended on its own ("Salir del juego").
    fn rl_host_game_ended(code: i32);
}

fn show_message(text: &str) {
    let text = CString::new(text.replace('\0', " ")).unwrap_or_default();
    unsafe { rl_host_show_message(text.as_ptr()) };
}

fn hide_message() {
    show_message("");
}

// ---------------------------------------------------------------------------------------------
// Storage and logging.

/// The app's Documents folder. It is visible in the Files app and in iTunes/Finder file sharing,
/// which is how the game gets onto the phone and how the log gets off it.
static STORAGE_DIR: OnceLock<PathBuf> = OnceLock::new();
/// The bundled resources (touch artwork, MIDI soundfont, mod manifests).
static BUNDLE_DIR: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn storage_dir() -> PathBuf {
    STORAGE_DIR.get().cloned().unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn storage_path(name: &str) -> PathBuf {
    storage_dir().join(name)
}

fn bundle_assets() -> PathBuf {
    BUNDLE_DIR.get().cloned().unwrap_or_else(|| PathBuf::from(".")).join("assets")
}

/// Sends everything the engine prints to `Documents/engine.log`. An iOS app's stdout and stderr go
/// nowhere a player can reach, and that log is what explains a game that will not start. The
/// previous session's log is kept as `engine.prev.log`.
fn redirect_output_to_log() {
    let path = storage_path("engine.log");
    let _ = fs::rename(&path, storage_path("engine.prev.log"));
    let Ok(c_path) = CString::new(path.as_os_str().as_encoded_bytes()) else { return };
    // SAFETY: plain POSIX calls on a path we own; the descriptor is duplicated onto stdout and
    // stderr and then closed, so nothing leaks.
    unsafe {
        let fd = libc::open(c_path.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644);
        if fd < 0 {
            return
        }
        libc::dup2(fd, libc::STDOUT_FILENO);
        libc::dup2(fd, libc::STDERR_FILENO);
        libc::close(fd);
    }
}

fn logf(msg: &str) {
    eprintln!("[ios] {}", msg);
}

// ---------------------------------------------------------------------------------------------
// Settings files. None exist until a settings screen writes them; without them the built-in
// defaults apply. Same format as the Android launcher's, so its files can be copied over.

fn load_video() {
    let Ok(text) = fs::read_to_string(storage_path("video.conf")) else { return };
    for line in text.lines() {
        let Some((name, value)) = line.trim().split_once('=') else { continue };
        let value = value.trim();
        match name.trim() {
            "scaling" if value == "aspect" || value == "full" => std::env::set_var("OPENGMK_SCALING", value),
            "render" => {
                let numeric = value.parse::<u32>().map(|hz| hz > 0).unwrap_or(false);
                if matches!(value, "native" | "auto" | "uncapped") || numeric {
                    std::env::set_var("OPENGMK_RENDER", value);
                }
            },
            "detected_hz" => {
                if value.parse::<u32>().is_ok_and(|hz| hz > 0) {
                    std::env::set_var("OPENGMK_RENDER_HZ", value);
                }
            },
            "show_fps" => std::env::set_var("OPENGMK_SHOW_FPS", if value == "1" { "1" } else { "0" }),
            _ => (),
        }
    }
}

fn load_controls() {
    tc::set_sprite_opacity(1.0);
    let Ok(text) = fs::read_to_string(storage_path("controls.conf")) else { return };
    for line in text.lines() {
        let Some((name, value)) = line.trim().split_once('=') else { continue };
        if name.trim() == "opacity" {
            if let Ok(opacity) = value.trim().parse::<f32>() {
                tc::set_sprite_opacity(opacity);
            }
        }
    }
}

fn load_touch() {
    let Ok(text) = fs::read_to_string(storage_path("touch.conf")) else { return };
    tc::clear_layout();
    let mut positions: Vec<(String, f32, f32)> = Vec::new();
    let mut sizes: Vec<(String, f32)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue
        }
        let Some((name, value)) = line.split_once('=') else { continue };
        let (name, value) = (name.trim(), value.trim());
        match name {
            "style" => tc::set_overlay_style(match value {
                "classic" => tc::OverlayStyle::Classic,
                "none" => tc::OverlayStyle::None,
                _ => tc::OverlayStyle::New,
            }),
            "move" => tc::set_move_mode(if value == "dpad" { tc::MoveMode::Dpad } else { tc::MoveMode::Joystick }),
            "scale" => {
                if let Ok(scale) = value.parse::<f32>() {
                    tc::set_ui_scale(scale);
                }
            },
            "opacity" => {
                if let Ok(opacity) = value.parse::<f32>() {
                    tc::set_sprite_opacity(opacity);
                }
            },
            _ => {
                if let Some(target) = name.strip_prefix("pos.") {
                    let mut parts = value.split(',');
                    if let (Some(cx), Some(cy)) = (parts.next(), parts.next()) {
                        if let (Ok(cx), Ok(cy)) = (cx.trim().parse::<f32>(), cy.trim().parse::<f32>()) {
                            positions.push((target.to_string(), cx, cy));
                        }
                    }
                } else if let Some(target) = name.strip_prefix("size.") {
                    if let Ok(scale) = value.parse::<f32>() {
                        sizes.push((target.to_string(), scale));
                    }
                }
            },
        }
    }
    for (name, cx, cy) in &positions {
        let scale = sizes.iter().find(|(other, _)| other == name).map(|(_, scale)| *scale).unwrap_or(1.0);
        tc::set_placement(name, *cx, *cy, scale);
    }
    for (name, scale) in &sizes {
        if positions.iter().any(|(other, _, _)| other == name) {
            continue
        }
        let (cx, cy) = tc::default_center(name).unwrap_or((0.5, 0.5));
        tc::set_placement(name, cx, cy, *scale);
    }
}

// ---------------------------------------------------------------------------------------------
// Touch input. A port of android_main.rs; see the comments there for the reasons behind the
// timings.

/// Which control each active touch is holding, keyed by touch id.
static POINTER_CONTROLS: Mutex<Option<HashMap<i32, usize>>> = Mutex::new(None);
/// The touch the party wheel is following, if any.
static WHEEL_PTR: Mutex<Option<i32>> = Mutex::new(None);
static FIELD_POINTER: Mutex<Option<(i32, usize)>> = Mutex::new(None);
/// Touches whose press was swallowed, so their release is swallowed too.
static SWALLOWED_INPUTS: Mutex<Option<HashSet<i32>>> = Mutex::new(None);
/// Keys tapped by the on-screen controls, waiting to be released.
static DELAYED_KEY_UP: Mutex<Vec<(Instant, Key)>> = Mutex::new(Vec::new());

/// A quick tap can deliver press and release inside one game step; holding the release this long
/// makes it a proper click.
const MIN_TAP: Duration = Duration::from_millis(70);
/// The game picks the menu entry under the cursor while it moves, so the move goes out first.
const HOVER_DELAY: Duration = Duration::from_millis(45);
const ROTATE_DELAY: Duration = Duration::from_millis(350);

static MOUSE_DOWN_AT: Mutex<Vec<(i32, Instant)>> = Mutex::new(Vec::new());
static DELAYED_UPS: Mutex<Vec<(Instant, f32, f32)>> = Mutex::new(Vec::new());
static DELAYED_DOWNS: Mutex<Vec<(Instant, f32, f32)>> = Mutex::new(Vec::new());
/// Last place the finger touched the game itself, not a button.
static LAST_GAME_TOUCH: Mutex<Option<(f32, f32)>> = Mutex::new(None);
static PENDING_ROTATE: Mutex<Option<(Instant, f32, f32)>> = Mutex::new(None);

/// Touch id currently driving the movement control, if any.
static MOVEMENT_PTR: Mutex<Option<i32>> = Mutex::new(None);
/// Which arrow keys the movement control holds (bit 0 up, 1 down, 2 left, 3 right).
static MOVEMENT_STATE: AtomicU32 = AtomicU32::new(0);
const MOVEMENT_KEYS: [Key; 4] = [Key::UpArrow, Key::DownArrow, Key::LeftArrow, Key::RightArrow];

fn push(event: InputEvent) {
    platform::push_input(event);
}

fn swallow(id: i32) {
    SWALLOWED_INPUTS.lock().unwrap().get_or_insert_with(HashSet::new).insert(id);
}

fn is_swallowed(id: i32) -> bool {
    SWALLOWED_INPUTS.lock().unwrap().as_ref().is_some_and(|ids| ids.contains(&id))
}

fn tap_key(key: Key) {
    if tc::key_held_by_buttons(key) || joystick_holds(key) {
        return
    }
    push(InputEvent::KeyDown { key, text: None });
    DELAYED_KEY_UP.lock().unwrap().push((Instant::now() + Duration::from_millis(30), key));
}

fn flush_delayed_key_ups() {
    let now = Instant::now();
    let mut released = Vec::new();
    DELAYED_KEY_UP.lock().unwrap().retain(|(at, key)| {
        if *at <= now {
            released.push(*key);
            false
        } else {
            true
        }
    });
    for key in released {
        push(InputEvent::KeyUp { key });
    }
}

fn joystick_holds(key: Key) -> bool {
    let state = MOVEMENT_STATE.load(Ordering::Relaxed);
    MOVEMENT_KEYS.iter().enumerate().any(|(i, k)| *k == key && state & (1 << i) != 0)
}

fn control_key_down(index: usize) {
    if tc::controls()[index].name == "o" && matches!(tc::current_context(), tc::WORLD | tc::BATTLE | tc::SAFARI) {
        crate::touch_navigation::arm_bag();
    } else {
        crate::touch_navigation::disarm_bag();
    }
    let keys = tc::control_keys(index);
    let held = keys.map(|key| key.is_some_and(|key| tc::key_held_by_buttons(key) || joystick_holds(key)));
    tc::set_pressed(index, true);
    for (i, key) in keys.into_iter().enumerate() {
        if let Some(key) = key.filter(|_| !held[i]) {
            match key {
                Key::LeftArrow => tc::picker_step(-1),
                Key::RightArrow => tc::picker_step(1),
                _ => {},
            }
            push(InputEvent::KeyDown { key, text: None });
        }
    }
    if tc::controls()[index].name == tc::COMBO_BUTTON_NAME {
        tc::set_combo_used();
    }
}

fn control_key_up(index: usize) {
    if !tc::is_pressed(index) {
        return
    }
    tc::set_pressed(index, false);
    for key in tc::control_keys(index).into_iter().flatten() {
        if !tc::key_held_by_buttons(key) && !joystick_holds(key) {
            push(InputEvent::KeyUp { key });
        }
    }
}

fn touch_feedback() {
    if crate::touch_appearance::settings().haptics {
        unsafe { rl_host_haptic() };
    }
}

fn pointer_down(id: i32, x: f32, y: f32) {
    if let Some(ids) = SWALLOWED_INPUTS.lock().unwrap().as_mut() {
        ids.remove(&id);
    }
    let (w, h) = platform::window_size();
    let (wf, hf) = (w as f32, h as f32);
    let on_movement = wf > 0.0 && hf > 0.0 && tc::movement_hit(x / wf, y / hf, wf, hf);
    // Any other touch while the party wheel is up closes it, except the movement stick.
    if tc::wheel_open() && *WHEEL_PTR.lock().unwrap() != Some(id) && !on_movement {
        tc::set_wheel_open(false);
        WHEEL_PTR.lock().unwrap().take();
    }
    if on_movement {
        *MOVEMENT_PTR.lock().unwrap() = Some(id);
        movement_update(x, y);
        return
    }
    let hit = if w > 0 && h > 0 { tc::hit(x / wf, y / hf, wf, hf) } else { None };
    if hit.is_some_and(|i| tc::controls()[i].name == crate::field_controls::NAME) {
        crate::field_controls::toggle();
        touch_feedback();
        swallow(id);
        return
    }
    if crate::field_controls::open() {
        if let Some(slot) = crate::field_controls::item_at(x, y, wf, hf) {
            let mut owner = FIELD_POINTER.lock().unwrap();
            if owner.is_none() {
                *owner = Some((id, slot));
                touch_feedback();
            }
        }
        swallow(id);
        return
    }
    if hit.is_none() && crate::touch_navigation::active() {
        if !crate::touch_navigation::begin(id, x, y, hf) {
            swallow(id);
        }
        return
    }
    match hit {
        Some(index) => {
            if tc::controls()[index].name == tc::ROTATE_NAME {
                queue_rotate_at_last_game_touch();
                swallow(id);
                touch_feedback();
                return
            }
            if tc::controls()[index].name == tc::PARTY_NAME {
                // Holding it opens the party wheel instead of pressing a key of its own.
                tc::set_wheel_open(true);
                *WHEEL_PTR.lock().unwrap() = Some(id);
            } else {
                let first = {
                    let mut pointers = POINTER_CONTROLS.lock().unwrap();
                    let pointers = pointers.get_or_insert_with(HashMap::new);
                    let first = !pointers.values().any(|held| *held == index);
                    pointers.insert(id, index);
                    first
                };
                if first {
                    control_key_down(index);
                }
            }
            touch_feedback();
        },
        None => {
            // The game asks for text through its own dialog; touching it brings the keyboard back.
            if tc::text_entry_open() {
                ime_set(true);
            }
            mouse_down(id, x, y)
        },
    }
}

fn pointer_moved(id: i32, x: f32, y: f32) {
    if is_swallowed(id) || crate::touch_navigation::moved(id, x, y) {
        return
    }
    let wheel_ptr = *WHEEL_PTR.lock().unwrap();
    if wheel_ptr == Some(id) {
        let (w, h) = platform::window_size();
        let (wf, hf) = (w as f32, h as f32);
        if wf > 0.0 && hf > 0.0 {
            // Only highlight while the finger moves; the key waits for the release.
            let picked = tc::wheel_item_at(x / wf, y / hf, wf, hf);
            tc::set_wheel_selected(picked.map(|i| i as i32).unwrap_or(-1));
        }
    } else if is_movement_pointer(id) {
        movement_update(x, y);
    } else if !pointer_is_control(id) {
        *LAST_GAME_TOUCH.lock().unwrap() = Some((x, y));
        push(InputEvent::PointerMoved { x, y });
    }
}

fn pointer_up(id: i32, x: f32, y: f32) {
    if crate::touch_navigation::owns(id) {
        if let Some((x, y)) = crate::touch_navigation::end(id, x, y) {
            push(InputEvent::PointerMoved { x, y });
        }
        return
    }
    let selected = {
        let mut owner = FIELD_POINTER.lock().unwrap();
        if owner.is_some_and(|(pointer, _)| pointer == id) { owner.take().map(|(_, slot)| slot) } else { None }
    };
    if let Some(slot) = selected {
        let (w, h) = platform::window_size();
        if crate::field_controls::open() && crate::field_controls::item_at(x, y, w as f32, h as f32) == Some(slot) {
            tap_key(crate::field_controls::KEYS[slot]);
            crate::field_controls::close();
        }
    }

    let closes_wheel = {
        let mut owner = WHEEL_PTR.lock().unwrap();
        if *owner == Some(id) {
            *owner = None;
            true
        } else {
            false
        }
    };
    if closes_wheel {
        // The key goes out on the release, and only if a slot was picked.
        let selected = tc::wheel_selected();
        if selected >= 0 {
            tap_key(tc::PICKER_KEYS[selected as usize]);
        }
        tc::set_wheel_open(false);
        return
    }
    if SWALLOWED_INPUTS.lock().unwrap().get_or_insert_with(HashSet::new).remove(&id) {
        return
    }
    if is_movement_pointer(id) {
        movement_release();
        return
    }
    let index = POINTER_CONTROLS.lock().unwrap().get_or_insert_with(HashMap::new).remove(&id);
    match index {
        Some(index) => {
            let still_held =
                POINTER_CONTROLS.lock().unwrap().as_ref().is_some_and(|p| p.values().any(|held| *held == index));
            if !still_held {
                control_key_up(index);
            }
        },
        None => mouse_up(id, x, y),
    }
}

/// iOS cancels touches when the system takes over (a notification pulled down, an incoming call).
/// Whatever the fingers were holding is released.
fn pointers_cancelled() {
    release_touch_profile();
    if let Some(ids) = SWALLOWED_INPUTS.lock().unwrap().as_mut() {
        ids.clear();
    }
    movement_release();
    let ids: Vec<i32> = POINTER_CONTROLS.lock().unwrap().as_ref().map(|m| m.keys().copied().collect()).unwrap_or_default();
    for id in ids {
        if let Some(index) = POINTER_CONTROLS.lock().unwrap().get_or_insert_with(HashMap::new).remove(&id) {
            control_key_up(index);
        }
    }
}

fn mouse_down(id: i32, x: f32, y: f32) {
    *LAST_GAME_TOUCH.lock().unwrap() = Some((x, y));
    let deliver_at = Instant::now() + HOVER_DELAY;
    {
        let mut downs = MOUSE_DOWN_AT.lock().unwrap();
        downs.retain(|(pointer, _)| *pointer != id);
        downs.push((id, deliver_at));
    }
    // The move goes out now; the press follows on a later tick so the hover is in place first.
    push(InputEvent::PointerMoved { x, y });
    DELAYED_DOWNS.lock().unwrap().push((deliver_at, x, y));
}

fn mouse_up(id: i32, x: f32, y: f32) {
    let pressed_at = {
        let mut downs = MOUSE_DOWN_AT.lock().unwrap();
        let at = downs.iter().find(|(pointer, _)| *pointer == id).map(|(_, at)| *at);
        downs.retain(|(pointer, _)| *pointer != id);
        at
    };
    let deadline = pressed_at.map(|at| at + MIN_TAP).unwrap_or_else(Instant::now);
    if deadline > Instant::now() {
        DELAYED_UPS.lock().unwrap().push((deadline, x, y));
    } else {
        push(InputEvent::PointerUp { x, y, button: 0 });
    }
}

fn right_click_at(x: f32, y: f32) {
    push(InputEvent::PointerMoved { x, y });
    push(InputEvent::PointerDown { x, y, button: 2 });
    push(InputEvent::PointerUp { x, y, button: 2 });
}

fn queue_rotate_at_last_game_touch() {
    if let Some((x, y)) = *LAST_GAME_TOUCH.lock().unwrap() {
        *PENDING_ROTATE.lock().unwrap() = Some((Instant::now() + ROTATE_DELAY, x, y));
    }
}

fn flush_pending_rotate() {
    let mut pending = PENDING_ROTATE.lock().unwrap();
    let Some((at, x, y)) = *pending else { return };
    if Instant::now() < at {
        return
    }
    *pending = None;
    drop(pending);
    right_click_at(x, y);
}

/// Android shows a hint for the rotating puzzle; there is no such screen here yet.
pub fn show_puzzle_hint() {
    logf("pista del puzzle (girar con el boton de girar)");
}

fn flush_delayed_mouse() {
    let now = Instant::now();
    let mut downs: Vec<(f32, f32)> = Vec::new();
    DELAYED_DOWNS.lock().unwrap().retain(|(at, x, y)| {
        if *at <= now {
            downs.push((*x, *y));
            false
        } else {
            true
        }
    });
    for (x, y) in downs {
        push(InputEvent::PointerDown { x, y, button: 0 });
    }
    let mut ups: Vec<(f32, f32)> = Vec::new();
    DELAYED_UPS.lock().unwrap().retain(|(at, x, y)| {
        if *at <= now {
            ups.push((*x, *y));
            false
        } else {
            true
        }
    });
    for (x, y) in ups {
        push(InputEvent::PointerUp { x, y, button: 0 });
    }
}

fn pointer_is_control(id: i32) -> bool {
    POINTER_CONTROLS.lock().unwrap().as_ref().map(|m| m.contains_key(&id)).unwrap_or(false)
}

fn is_movement_pointer(id: i32) -> bool {
    *MOVEMENT_PTR.lock().unwrap() == Some(id)
}

/// Presses and releases the arrow keys so they match where the movement control is pushed.
fn movement_update(x: f32, y: f32) {
    crate::touch_navigation::disarm_bag();
    let (w, h) = platform::window_size();
    let (wf, hf) = (w as f32, h as f32);
    if wf <= 0.0 || hf <= 0.0 {
        return
    }
    let dirs = tc::movement_dirs(x / wf, y / hf, wf, hf);
    tc::set_move_dirs(dirs);
    let mut state = MOVEMENT_STATE.load(Ordering::Relaxed);
    let mut new_direction = false;
    for (i, &on) in dirs.iter().enumerate() {
        let bit = 1u32 << i;
        if on != (state & bit != 0) {
            if !tc::key_held_by_buttons(MOVEMENT_KEYS[i]) {
                push(if on {
                    InputEvent::KeyDown { key: MOVEMENT_KEYS[i], text: None }
                } else {
                    InputEvent::KeyUp { key: MOVEMENT_KEYS[i] }
                });
            }
            if on {
                new_direction = true;
                state |= bit;
            } else {
                state &= !bit;
            }
        }
    }
    MOVEMENT_STATE.store(state, Ordering::Relaxed);
    if new_direction && tc::move_mode() == tc::MoveMode::Dpad {
        touch_feedback();
    }

    // Clamp the drawn knob to the ring.
    let classic = tc::overlay_style() == tc::OverlayStyle::Classic;
    let (jcx, jcy) = if classic { tc::joystick_center_in_window(wf, hf) } else { tc::movement_center() };
    let r = if classic { tc::joystick_radius() } else { tc::movement_radius() } * hf;
    let mut dx = (x - jcx * wf) / r;
    let mut dy = (y - jcy * hf) / r;
    let len = (dx * dx + dy * dy).sqrt();
    if len > 1.0 {
        dx /= len;
        dy /= len;
    }
    tc::set_movement_offset(dx, dy);
}

fn movement_release() {
    tc::set_move_dirs([false; 4]);
    let state = MOVEMENT_STATE.swap(0, Ordering::Relaxed);
    for (i, key) in MOVEMENT_KEYS.iter().enumerate() {
        if state & (1 << i) != 0 && !tc::key_held_by_buttons(*key) {
            push(InputEvent::KeyUp { key: *key });
        }
    }
    tc::set_movement_offset(0.0, 0.0);
    *MOVEMENT_PTR.lock().unwrap() = None;
}

/// Lets go of everything the fingers hold, when the game moves to a screen with other controls.
fn release_touch_profile() {
    let held = POINTER_CONTROLS.lock().unwrap().take().unwrap_or_default();
    let mut ids = Vec::new();
    for (id, index) in held {
        control_key_up(index);
        ids.push(id);
    }
    if let Some(id) = *MOVEMENT_PTR.lock().unwrap() {
        ids.push(id);
    }
    movement_release();
    if let Some(id) = WHEEL_PTR.lock().unwrap().take() {
        ids.push(id);
    }
    tc::set_wheel_open(false);
    crate::field_controls::close();
    if let Some((id, _)) = FIELD_POINTER.lock().unwrap().take() {
        ids.push(id);
    }
    if let Some(id) = crate::touch_navigation::cancel() {
        ids.push(id);
    }
    if let Some((x, y)) = crate::touch_navigation::cancel_pointer() {
        push(InputEvent::PointerUp { x, y, button: 0 });
    }
    let mut swallowed = SWALLOWED_INPUTS.lock().unwrap();
    swallowed.get_or_insert_with(HashSet::new).extend(ids);
}

fn reset_input_state() {
    *POINTER_CONTROLS.lock().unwrap() = None;
    *WHEEL_PTR.lock().unwrap() = None;
    *FIELD_POINTER.lock().unwrap() = None;
    *SWALLOWED_INPUTS.lock().unwrap() = None;
    MOUSE_DOWN_AT.lock().unwrap().clear();
    DELAYED_UPS.lock().unwrap().clear();
    DELAYED_DOWNS.lock().unwrap().clear();
    *MOVEMENT_PTR.lock().unwrap() = None;
    MOVEMENT_STATE.store(0, Ordering::Relaxed);
    IME_SHOWN.store(false, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------------------------
// System keyboard. The app's view adopts UIKeyInput and forwards what is typed; `ime_pump` turns
// it into key taps right before the game polls, the same way Android does.

enum ImeEvent {
    Text(String),
    Backspace,
    Enter,
}

fn ime_queue() -> &'static Mutex<std::collections::VecDeque<ImeEvent>> {
    static QUEUE: OnceLock<Mutex<std::collections::VecDeque<ImeEvent>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

/// Feeds what the keyboard typed into the game's input queue. Called by the engine every poll.
pub fn ime_pump() {
    let events: Vec<ImeEvent> = match ime_queue().lock() {
        Ok(mut queue) => queue.drain(..).collect(),
        Err(_) => return,
    };
    for event in events {
        match event {
            ImeEvent::Text(text) => {
                for c in text.chars() {
                    if c == '\n' || c == '\r' {
                        tap_key(Key::Return);
                        continue
                    }
                    let key = ime_char_to_key(c);
                    push(InputEvent::KeyDown { key, text: Some(c) });
                    push(InputEvent::KeyUp { key });
                }
            },
            ImeEvent::Backspace => tap_key(Key::Backspace),
            ImeEvent::Enter => tap_key(Key::Return),
        }
    }
}

const LETTERS: [Key; 26] = [
    Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H, Key::I, Key::J, Key::K, Key::L, Key::M,
    Key::N, Key::O, Key::P, Key::Q, Key::R, Key::S, Key::T, Key::U, Key::V, Key::W, Key::X, Key::Y, Key::Z,
];

/// The key a typed character is reported as. It only decides `keyboard_lastkey`; the character
/// itself travels in the event's text.
fn ime_char_to_key(c: char) -> Key {
    if c.is_ascii_alphabetic() {
        return LETTERS[usize::from(c.to_ascii_uppercase() as u8 - b'A')]
    }
    if c.is_ascii_digit() {
        const DIGITS: [Key; 10] = [
            Key::Alpha0, Key::Alpha1, Key::Alpha2, Key::Alpha3, Key::Alpha4, Key::Alpha5, Key::Alpha6, Key::Alpha7,
            Key::Alpha8, Key::Alpha9,
        ];
        return DIGITS[usize::from(c as u8 - b'0')]
    }
    match c {
        ' ' => Key::Space,
        '.' => Key::Period,
        ',' => Key::Comma,
        '-' => Key::Minus,
        '+' => Key::Plus,
        '=' => Key::Equals,
        '/' => Key::Slash,
        '\\' => Key::Backslash,
        ';' => Key::Semicolon,
        ':' => Key::Colon,
        '\'' => Key::Apostrophe,
        '"' => Key::Quote,
        '?' => Key::QuestionMark,
        '_' => Key::Underscore,
        _ => Key::Space,
    }
}

/// Whether the keyboard is up, so it is only raised or dropped on a change.
static IME_SHOWN: AtomicBool = AtomicBool::new(false);

fn ime_set(visible: bool) {
    if IME_SHOWN.swap(visible, Ordering::Relaxed) != visible {
        unsafe { rl_host_set_keyboard(visible) };
    }
}

// ---------------------------------------------------------------------------------------------
// Reports the Android app turns into Discord presence. Nothing listens on iOS.

pub(crate) fn report_route(_room_id: i32, _name: impl FnOnce() -> String) {}
pub(crate) fn report_character(_prota: i32, _outfit: i32, _char2: i32) {}
pub(crate) fn report_player_name(_name: &str) {}
pub(crate) fn report_battle(_in_battle: bool, _trainer: &str, _species: i32) {}
pub(crate) fn reset_player_reports() {}

/// iOS never asks a running session to make way for another one, unlike a recreated Activity.
pub fn worker_stop_requested() -> bool {
    false
}

// ---------------------------------------------------------------------------------------------
// Getting the game ready. The player copies the game to the app's Documents folder, either as the
// .zip from the blog or as the unpacked folder. A zip is unpacked once into `game/`.

const GAME_DIR: &str = "game";

/// The `.exe` directly inside `dir`, if there is one. Its name is not pinned, so another version of
/// the game works unchanged.
fn find_exe(dir: &Path) -> Option<PathBuf> {
    let mut found = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("exe")))
        .collect::<Vec<_>>();
    found.sort();
    found.into_iter().next()
}

fn is_game_root(dir: &Path) -> bool {
    find_exe(dir).is_some() && dir.join("data").is_dir()
}

/// The folder holding the `.exe` and `data/`: `dir` itself, or a folder up to two levels below it
/// (zips usually wrap everything in a folder named after the game).
fn find_game_root(dir: &Path) -> Option<PathBuf> {
    if is_game_root(dir) {
        return Some(dir.to_path_buf())
    }
    let subdirs = |dir: &Path| -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = fs::read_dir(dir)
            .map(|entries| entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect())
            .unwrap_or_default();
        dirs.sort();
        dirs
    };
    for child in subdirs(dir) {
        if is_game_root(&child) {
            return Some(child)
        }
        for grandchild in subdirs(&child) {
            if is_game_root(&grandchild) {
                return Some(grandchild)
            }
        }
    }
    None
}

/// What the app keeps in Documents itself: the unpacked game, its staging copies, the touch artwork.
fn is_app_owned(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == GAME_DIR || n.starts_with("game.") || n.starts_with("touch-"))
}

/// Whether the file starts like a zip archive. Players rename the download ("POKEMON"), and the
/// Files app may drop the extension along the way, so the name alone is not enough.
fn looks_like_zip(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    fs::File::open(path).and_then(|mut file| file.read_exact(&mut magic)).is_ok() && magic == *b"PK\x03\x04"
}

/// Zips in Documents or in a folder up to two levels below it. `game.zip` comes first, then files
/// named `.zip`, then the rest.
fn zip_candidates(storage: &Path) -> Vec<PathBuf> {
    fn collect(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
            if is_app_owned(&path) {
                continue
            }
            if path.is_dir() {
                if depth < 2 {
                    collect(&path, depth + 1, out);
                }
            } else if looks_like_zip(&path) {
                out.push(path);
            }
        }
    }
    let mut zips = Vec::new();
    collect(storage, 0, &mut zips);
    zips.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_ascii_lowercase();
        (name != "game.zip", !name.ends_with(".zip"), p.clone())
    });
    zips
}

fn free_bytes_on(path: &Path) -> Option<u64> {
    // SAFETY: a zeroed statvfs is a valid argument, and the call only reads the path.
    unsafe {
        let path_c = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        let mut stats: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path_c.as_ptr(), &mut stats) != 0 {
            return None
        }
        Some(stats.f_bavail as u64 * stats.f_frsize as u64)
    }
}

/// Unpacks `zip_path` into `game/` and returns the game's folder.
fn extract_zip(zip_path: &Path, storage: &Path) -> Result<PathBuf, String> {
    let game_dir = storage.join(GAME_DIR);
    let file = fs::File::open(zip_path).map_err(|e| format!("{}: {}", zip_path.display(), e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| {
        format!("no pude leer {} como zip ({}). Si es un .rar o .7z, conviertelo a .zip en el PC.", zip_path.display(), e)
    })?;
    let mut uncompressed = 0u64;
    for index in 0..archive.len() {
        if let Ok(entry) = archive.by_index_raw(index) {
            uncompressed += entry.size();
        }
    }
    if let Some(free) = free_bytes_on(storage) {
        if free < uncompressed + 50 * 1024 * 1024 {
            return Err(format!(
                "no hay espacio: el juego ocupa unos {} MB descomprimido y quedan {} MB libres",
                uncompressed / (1024 * 1024),
                free / (1024 * 1024)
            ))
        }
    }

    show_message(&format!(
        "Descomprimiendo el juego ({} MB)...\nPuede tardar un par de minutos. No cierres la app.",
        uncompressed / (1024 * 1024)
    ));
    // Unpack into a staging folder and swap it in, so a failed run leaves no half-unpacked game.
    let staging = storage.join("game.staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("no se pudo crear {}: {}", staging.display(), e))?;
    logf(&format!("extrayendo {} ({} entradas)", zip_path.display(), archive.len()));
    archive.extract(&staging).map_err(|e| format!("fallo al descomprimir {}: {}", zip_path.display(), e))?;

    let Some(root) = find_game_root(&staging) else {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("{} no contiene el juego (falta el .exe junto a la carpeta data/)", zip_path.display()))
    };
    let previous = storage.join("game.old");
    let _ = fs::remove_dir_all(&previous);
    let moved_aside = game_dir.is_dir() && fs::rename(&game_dir, &previous).is_ok();
    if let Err(e) = fs::rename(&root, &game_dir) {
        if moved_aside {
            let _ = fs::rename(&previous, &game_dir);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("no pude mover lo descomprimido a {}: {}", game_dir.display(), e))
    }
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_dir_all(&previous);
    logf(&format!("juego listo en {}", game_dir.display()));
    Ok(game_dir)
}

/// Writes what Documents holds (two levels deep) to the log, so a game that was not found can be
/// tracked down from engine.log alone.
fn log_documents(storage: &Path) {
    fn walk(dir: &Path, depth: usize, budget: &mut usize) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if *budget == 0 {
                logf("  ...");
                return
            }
            *budget -= 1;
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let indent = "  ".repeat(depth + 1);
            if path.is_dir() {
                logf(&format!("{}{}/", indent, name));
                if depth < 1 && !is_app_owned(&path) {
                    walk(&path, depth + 1, budget);
                }
            } else {
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let kind = if looks_like_zip(&path) { ", zip" } else { "" };
                logf(&format!("{}{} ({} KB{})", indent, name, size / 1024, kind));
            }
        }
    }
    logf(&format!("contenido de {}:", storage.display()));
    walk(storage, 0, &mut 60);
}

/// Finds the game, unpacking it first if it arrived as a zip. Returns the path of its `.exe`.
fn ensure_game_ready() -> Result<PathBuf, String> {
    let storage = storage_dir();
    let game_dir = storage.join(GAME_DIR);
    if is_game_root(&game_dir) {
        return find_exe(&game_dir).ok_or_else(|| "no encuentro el .exe".to_string())
    }
    // An unpacked folder copied over as it is. It is moved to `game/` (a rename, so instant): the
    // touch overlay reads the party sprites from there.
    if let Ok(entries) = fs::read_dir(&storage) {
        let mut dirs: Vec<PathBuf> =
            entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir() && !is_app_owned(p)).collect();
        dirs.sort();
        for dir in dirs {
            let Some(root) = find_game_root(&dir) else { continue };
            logf(&format!("carpeta del juego encontrada en {}", root.display()));
            if game_dir.exists() {
                let _ = fs::remove_dir_all(storage.join("game.old"));
                let _ = fs::rename(&game_dir, storage.join("game.old"));
            }
            fs::rename(&root, &game_dir)
                .map_err(|e| format!("no pude mover {} a {}: {}", root.display(), game_dir.display(), e))?;
            return find_exe(&game_dir).ok_or_else(|| "no encuentro el .exe".to_string())
        }
    }
    let zips = zip_candidates(&storage);
    let Some(zip) = zips.first() else {
        return Err(concat!(
            "No encuentro el juego.\n\n",
            "1. Descarga Pokemon Reloaded del blog oficial (pokemon-reloaded.blogspot.com).\n",
            "2. Copia el .zip a esta app: en la app Archivos, \"En mi iPhone\" > \"Reloaded\", ",
            "o desde el PC con iTunes > iPhone > Archivos compartidos.\n",
            "3. Cierra la app del todo y vuelve a abrirla."
        )
        .to_string())
    };
    let root = extract_zip(zip, &storage)?;
    find_exe(&root).ok_or_else(|| "no encuentro el .exe tras descomprimir".to_string())
}

/// Copies what the Android launcher would have provided: the touch controls' artwork (refreshed
/// every start, so an app update takes effect), the mod manifests, and the MIDI soundfont.
fn provision_bundled_assets(game_dir: &Path) {
    let assets = bundle_assets();
    let storage = storage_dir();
    if let Ok(entries) = fs::read_dir(&assets) {
        for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
            if path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("png")) {
                if let Some(name) = path.file_name() {
                    let _ = fs::copy(&path, storage.join(name));
                }
            }
        }
    } else {
        logf(&format!("aviso: no encuentro los recursos de la app en {}", assets.display()));
    }

    let mods = game_dir.join("mods");
    for id in ["gb", "crt"] {
        let target = mods.join(id).join("mod.ini");
        if !target.is_file() {
            let _ = fs::create_dir_all(mods.join(id));
            let _ = fs::copy(assets.join("mods").join(id).join("mod.ini"), &target);
        }
    }
    // Mods are opt-in, as on Android.
    if !mods.join("enabled.flag").is_file() && !mods.join("disabled.flag").is_file() {
        let _ = fs::write(mods.join("disabled.flag"), "1");
    }

    // MIDI music needs a General MIDI soundfont. One next to the game wins; otherwise the bundled
    // one is read from the app itself, with no copy.
    let candidates = [game_dir.join("soundfont.sf2"), game_dir.join("TimGM6mb.sf2"), assets.join("soundfont.sf2")];
    match candidates.iter().find(|path| path.is_file()) {
        Some(path) => {
            logf(&format!("soundfont MIDI: {}", path.display()));
            std::env::set_var("OPENGMK_SOUNDFONT", path);
        },
        None => logf("aviso: no hay .sf2; la musica MIDI no sonara"),
    }
}

// ---------------------------------------------------------------------------------------------
// Entry points for the app. All are called on the main thread.

static STARTED: AtomicBool = AtomicBool::new(false);

/// Starts the engine on its own thread. `documents` and `bundle` are UTF-8 paths; `layer` is the
/// view's `CAEAGLLayer`; `width` and `height` are its size in pixels.
///
/// # Safety
/// The paths must be valid NUL-terminated strings and `layer` must stay alive for the app's life.
#[no_mangle]
pub unsafe extern "C" fn rl_ios_start(
    documents: *const c_char,
    bundle: *const c_char,
    layer: *mut c_void,
    width: u32,
    height: u32,
) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return
    }
    // SAFETY: the caller passes valid NUL-terminated strings.
    let path_from = |ptr: *const c_char| PathBuf::from(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned());
    let _ = STORAGE_DIR.set(path_from(documents));
    let _ = BUNDLE_DIR.set(path_from(bundle));
    redirect_output_to_log();
    std::env::set_var("RUST_BACKTRACE", "1");
    logf(&format!("Documents: {}", storage_dir().display()));
    logf(&format!("pantalla: {}x{} px", width, height));

    reset_input_state();
    load_controls();
    load_touch();
    load_video();
    tc::load_layout(&fs::read_to_string(storage_path("touch-layout.conf")).unwrap_or_default());
    crate::touch_appearance::load(&fs::read_to_string(storage_path("touch-appearance.conf")).unwrap_or_default());

    platform::set_native_window(layer, width, height);

    let spawned = std::thread::Builder::new()
        .name("gm8emulator".into())
        // The interpreter recurses through GML scripts; the 512 KiB a secondary iOS thread gets by
        // default is not enough for it.
        .stack_size(16 * 1024 * 1024)
        .spawn(run_worker);
    if let Err(e) = spawned {
        logf(&format!("no pude crear el hilo del juego: {}", e));
        show_message("No se pudo arrancar el juego (no se pudo crear el hilo).");
    }
}

fn run_worker() {
    let game_exe = match ensure_game_ready() {
        Ok(exe) => exe,
        Err(e) => {
            logf(&format!("no se pudo preparar el juego: {}", e));
            log_documents(&storage_dir());
            show_message(&e);
            return
        },
    };
    provision_bundled_assets(game_exe.parent().unwrap_or_else(|| Path::new("/")));
    hide_message();
    logf(&format!("arrancando {}", game_exe.display()));
    let outcome = std::panic::catch_unwind(|| {
        crate::run_game(vec!["gm8emulator".to_string(), game_exe.to_string_lossy().into_owned()])
    });
    crate::render::opengl::egl::log_stubbed_once();
    match outcome {
        Ok(code) => {
            logf(&format!("el juego termino (codigo {})", code));
            if code == 0 {
                unsafe { rl_host_game_ended(code) };
            } else {
                show_message(&format!(
                    "El juego se ha cerrado (codigo {}).\n\nEl motivo esta en engine.log, en la carpeta de esta app (app Archivos).",
                    code
                ));
            }
        },
        Err(_) => {
            logf("el emulador se cayo (panic)");
            show_message(
                "El juego se ha cerrado por un error.\n\nEl detalle esta en engine.log, en la carpeta de esta app (app Archivos).",
            );
        },
    }
}

/// The view changed size (first layout, or the safe area moved).
#[no_mangle]
pub extern "C" fn rl_ios_resize(width: u32, height: u32) {
    if width == 0 || height == 0 {
        return
    }
    platform::push_resize(width.min(u32::from(u16::MAX)) as u16, height.min(u32::from(u16::MAX)) as u16);
    crate::render::opengl::egl::request_resize();
}

/// The app came to the foreground (`true`) or is leaving it (`false`). On leaving, this returns only
/// once no frame is being drawn, because iOS kills apps that use OpenGL ES in the background.
#[no_mangle]
pub extern "C" fn rl_ios_set_active(active: bool) {
    if active {
        crate::render::opengl::egl::resume();
        platform::set_paused(false);
        crate::game::audio::set_output_paused(false);
    } else {
        // Whatever the fingers held is let go, or the player would keep walking on return.
        pointers_cancelled();
        platform::set_paused(true);
        crate::game::audio::set_output_paused(true);
        crate::render::opengl::egl::suspend();
    }
}

/// Another app took the audio (a call, an alarm) or gave it back.
#[no_mangle]
pub extern "C" fn rl_ios_audio_interrupted(interrupted: bool) {
    crate::game::audio::set_focus_paused(interrupted);
}

/// One touch, in pixels of the view. `phase`: 0 began, 1 moved, 2 ended, 3 cancelled.
#[no_mangle]
pub extern "C" fn rl_ios_touch(phase: i32, id: i32, x: f32, y: f32) {
    match phase {
        0 => pointer_down(id, x, y),
        1 => pointer_moved(id, x, y),
        2 => pointer_up(id, x, y),
        _ => pointers_cancelled(),
    }
}

/// Called once per display frame: delivers the held-back presses and releases, follows the game's
/// state changes, and raises or drops the keyboard with the game's text dialog.
#[no_mangle]
pub extern "C" fn rl_ios_tick() {
    static LAST_CONTEXT: Mutex<Option<(u8, crate::touch_navigation::Identity)>> = Mutex::new(None);
    static TEXT_ENTRY_WAS_OPEN: AtomicBool = AtomicBool::new(false);

    let now = (tc::current_context(), crate::touch_navigation::identity());
    let changed = {
        let mut last = LAST_CONTEXT.lock().unwrap();
        let changed = last.is_some_and(|last| last != now);
        *last = Some(now);
        changed
    };
    if changed {
        release_touch_profile();
    }
    if let Some(key) = crate::touch_navigation::next_key() {
        tap_key(key);
    }
    if let Some((down, x, y)) = crate::touch_navigation::next_pointer() {
        push(if down { InputEvent::PointerDown { x, y, button: 0 } } else { InputEvent::PointerUp { x, y, button: 0 } });
    }
    // Drop the keys of controls that are no longer shown, so none stays stuck down.
    for index in tc::stale_pressed() {
        control_key_up(index);
    }
    flush_delayed_mouse();
    flush_delayed_key_ups();
    flush_pending_rotate();
    // The keyboard follows the game's text dialog. Only the change is acted on, so closing it by
    // hand while the dialog is open is respected; touching the text brings it back.
    let text_entry = tc::text_entry_open();
    if TEXT_ENTRY_WAS_OPEN.swap(text_entry, Ordering::Relaxed) != text_entry {
        ime_set(text_entry);
    }
}

/// Text typed on the system keyboard (UTF-8).
///
/// # Safety
/// `text` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn rl_ios_commit_text(text: *const c_char) {
    if text.is_null() {
        return
    }
    let value = CStr::from_ptr(text).to_string_lossy().into_owned();
    if value.is_empty() {
        return
    }
    if let Ok(mut queue) = ime_queue().lock() {
        queue.push_back(ImeEvent::Text(value));
    }
}

/// The keyboard's delete key.
#[no_mangle]
pub extern "C" fn rl_ios_backspace() {
    if let Ok(mut queue) = ime_queue().lock() {
        queue.push_back(ImeEvent::Backspace);
    }
}

/// The keyboard's return key.
#[no_mangle]
pub extern "C" fn rl_ios_enter() {
    if let Ok(mut queue) = ime_queue().lock() {
        queue.push_back(ImeEvent::Enter);
    }
}

/// How many pixels of the view the keyboard covers (0 when it is down), so the game is kept above it.
#[no_mangle]
pub extern "C" fn rl_ios_ime_inset(height: u32) {
    let (_, window_height) = platform::window_size();
    let height = if window_height > 0 && height >= window_height { 0 } else { height };
    tc::set_ime_inset(height);
    IME_SHOWN.store(height > 0, Ordering::Relaxed);
}
