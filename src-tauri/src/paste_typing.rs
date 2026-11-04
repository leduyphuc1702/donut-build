//! Paste as typing: a global shortcut (Ctrl+Alt+V, ⌘⌥V on macOS) types the
//! clipboard into the field that has the focus, one key at a time with a
//! person's rhythm, so a site sees typing instead of a paste.
//!
//! Keys go through the OS (SendInput on Windows, CGEvent on macOS) as the
//! physical keys of a US keyboard, each held down for a moment and then let
//! go, with Shift for capitals and symbols. Nothing goes through a browser's
//! automation port. Typos the plan makes are always corrected, so the field
//! ends up with exactly the copied text. Escape or a mouse click stops it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rand::RngExt;
use tauri::AppHandle;

use crate::human_typing::{MarkovTyper, TypingAction};

/// The longest text it types: more is almost surely not one form field.
pub const MAX_CHARS: usize = 500;
/// People type copied numbers and addresses slower than prose.
const WPM: f64 = 45.0;
/// Reading the next group of a long number (a card, a phone) takes a moment.
const GROUP_PAUSE_MEAN: f64 = 0.35;
const GROUP_PAUSE_STD: f64 = 0.1;
/// How long a key stays down: a keyboard is never pressed and let go at once.
const DWELL_MEAN_MS: f64 = 85.0;
const DWELL_STD_MS: f64 = 20.0;
/// How long the shortcut's own keys may still be held before typing starts.
const KEYS_HELD_LIMIT: Duration = Duration::from_secs(3);

static TYPING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stroke {
  Char(char),
  Backspace,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlannedStroke {
  /// When the key goes down, from the start.
  pub at: Duration,
  pub stroke: Stroke,
}

/// What gets typed: the clipboard text on one line. Line breaks and tabs
/// would press Enter or Tab in a form, so they become spaces.
pub fn sanitize(text: &str) -> Result<String, String> {
  let one_line = text
    .split(['\r', '\n', '\t'])
    .map(str::trim)
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ");
  if one_line.is_empty() {
    return Err(crate::backend_error("PASTE_TYPING_EMPTY"));
  }
  if one_line.chars().count() > MAX_CHARS {
    return Err(
      serde_json::json!({
        "code": "PASTE_TYPING_TOO_LONG",
        "params": { "max": MAX_CHARS.to_string() },
      })
      .to_string(),
    );
  }
  Ok(one_line)
}

fn normal(rng: &mut impl rand::Rng, mean: f64, std_dev: f64) -> f64 {
  let u1: f64 = rng.random::<f64>().max(1e-10);
  let u2: f64 = rng.random::<f64>();
  mean + std_dev * (-2.0_f64 * u1.ln()).sqrt() * (2.0_f64 * std::f64::consts::PI * u2).cos()
}

/// The key presses that type `text`: the human-typing plan, with its typos
/// and their corrections, and a pause before each group of four digits.
/// A plan that would not end on exactly `text` is drawn again; after a few
/// tries the text is typed without typos.
pub fn plan(text: &str) -> Vec<PlannedStroke> {
  let target: Vec<char> = text.chars().collect();
  for _ in 0..5 {
    let events = MarkovTyper::new(text, Some(WPM)).run();
    let strokes: Vec<(f64, Stroke)> = events
      .iter()
      .map(|event| {
        let stroke = match event.action {
          TypingAction::Char(c) => Stroke::Char(c),
          TypingAction::Backspace => Stroke::Backspace,
        };
        (event.time, stroke)
      })
      .collect();
    if replay(strokes.iter().map(|(_, s)| *s)) == text {
      return with_group_pauses(&target, strokes);
    }
  }
  let mut rng = rand::rng();
  let mut at = 0.0;
  let strokes = target
    .iter()
    .map(|c| {
      at += normal(&mut rng, 60.0 / (WPM * 5.0), 0.04).max(0.05);
      (at, Stroke::Char(*c))
    })
    .collect();
  with_group_pauses(&target, strokes)
}

fn with_group_pauses(target: &[char], strokes: Vec<(f64, Stroke)>) -> Vec<PlannedStroke> {
  let mut rng = rand::rng();
  let mut shift = 0.0;
  // How many characters the field holds before each stroke.
  let mut typed = 0usize;
  strokes
    .into_iter()
    .map(|(time, stroke)| {
      match stroke {
        Stroke::Char(c) => {
          let digits_before = target[..typed.min(target.len())]
            .iter()
            .rev()
            .take_while(|d| d.is_ascii_digit())
            .count();
          let next_group = c.is_ascii_digit()
            && target.get(typed) == Some(&c)
            && digits_before > 0
            && digits_before % 4 == 0;
          if next_group {
            shift += normal(&mut rng, GROUP_PAUSE_MEAN, GROUP_PAUSE_STD).max(0.1);
          }
          typed += 1;
        }
        Stroke::Backspace => typed = typed.saturating_sub(1),
      }
      PlannedStroke {
        at: Duration::from_secs_f64(time + shift),
        stroke,
      }
    })
    .collect()
}

/// The text a field holds after `strokes`.
pub fn replay(strokes: impl IntoIterator<Item = Stroke>) -> String {
  let mut text = String::new();
  for stroke in strokes {
    match stroke {
      Stroke::Char(c) => text.push(c),
      Stroke::Backspace => {
        text.pop();
      }
    }
  }
  text
}

/// The key of a US keyboard that makes `c`, and whether Shift is held for it.
/// `None` for anything else, which is entered as text.
pub fn us_key(c: char) -> Option<(char, bool)> {
  const SHIFTED: [(char, char); 21] = [
    ('~', '`'),
    ('!', '1'),
    ('@', '2'),
    ('#', '3'),
    ('$', '4'),
    ('%', '5'),
    ('^', '6'),
    ('&', '7'),
    ('*', '8'),
    ('(', '9'),
    (')', '0'),
    ('_', '-'),
    ('+', '='),
    ('{', '['),
    ('}', ']'),
    ('|', '\\'),
    (':', ';'),
    ('"', '\''),
    ('<', ','),
    ('>', '.'),
    ('?', '/'),
  ];
  match c {
    'a'..='z' | '0'..='9' | ' ' => Some((c, false)),
    'A'..='Z' => Some((c.to_ascii_lowercase(), true)),
    '`' | '-' | '=' | '[' | ']' | '\\' | ';' | '\'' | ',' | '.' | '/' => Some((c, false)),
    _ => SHIFTED
      .iter()
      .find(|(shifted, _)| *shifted == c)
      .map(|(_, base)| (*base, true)),
  }
}

/// The OS key for a US keyboard key.
#[cfg(target_os = "macos")]
fn os_key(base: char) -> enigo::Key {
  // kVK_ANSI_* from HIToolbox Events.h: positions, the same on every layout.
  let code: u32 = match base {
    'a' => 0x00,
    's' => 0x01,
    'd' => 0x02,
    'f' => 0x03,
    'h' => 0x04,
    'g' => 0x05,
    'z' => 0x06,
    'x' => 0x07,
    'c' => 0x08,
    'v' => 0x09,
    'b' => 0x0B,
    'q' => 0x0C,
    'w' => 0x0D,
    'e' => 0x0E,
    'r' => 0x0F,
    'y' => 0x10,
    't' => 0x11,
    '1' => 0x12,
    '2' => 0x13,
    '3' => 0x14,
    '4' => 0x15,
    '6' => 0x16,
    '5' => 0x17,
    '=' => 0x18,
    '9' => 0x19,
    '7' => 0x1A,
    '-' => 0x1B,
    '8' => 0x1C,
    '0' => 0x1D,
    ']' => 0x1E,
    'o' => 0x1F,
    'u' => 0x20,
    '[' => 0x21,
    'i' => 0x22,
    'p' => 0x23,
    'l' => 0x25,
    'j' => 0x26,
    '\'' => 0x27,
    'k' => 0x28,
    ';' => 0x29,
    '\\' => 0x2A,
    ',' => 0x2B,
    '/' => 0x2C,
    'n' => 0x2D,
    'm' => 0x2E,
    '.' => 0x2F,
    ' ' => 0x31,
    '`' => 0x32,
    _ => return enigo::Key::Unicode(base),
  };
  enigo::Key::Other(code)
}

#[cfg(target_os = "windows")]
fn os_key(base: char) -> enigo::Key {
  // Virtual-key codes of a US keyboard.
  let vk: u32 = match base {
    'a'..='z' => base.to_ascii_uppercase() as u32,
    '0'..='9' => base as u32,
    ' ' => 0x20,
    ';' => 0xBA,
    '=' => 0xBB,
    ',' => 0xBC,
    '-' => 0xBD,
    '.' => 0xBE,
    '/' => 0xBF,
    '`' => 0xC0,
    '[' => 0xDB,
    '\\' => 0xDC,
    ']' => 0xDD,
    '\'' => 0xDE,
    _ => return enigo::Key::Unicode(base),
  };
  enigo::Key::Other(vk)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn os_key(base: char) -> enigo::Key {
  if base == ' ' {
    enigo::Key::Space
  } else {
    enigo::Key::Unicode(base)
  }
}

#[cfg(target_os = "windows")]
mod keys {
  use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_LBUTTON, VK_LWIN, VK_MENU, VK_RBUTTON,
    VK_RWIN, VK_SHIFT,
  };

  fn down(vk: VIRTUAL_KEY) -> bool {
    (unsafe { GetAsyncKeyState(i32::from(vk.0)) } as u16 & 0x8000) != 0
  }

  pub fn modifiers_held() -> bool {
    [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
      .into_iter()
      .any(down)
  }

  pub fn interrupted() -> bool {
    [VK_ESCAPE, VK_LBUTTON, VK_RBUTTON].into_iter().any(down)
  }
}

#[cfg(target_os = "macos")]
mod keys {
  const HID_SYSTEM_STATE: i32 = 1;
  const MODIFIERS: u64 = 0x0002_0000 | 0x0004_0000 | 0x0008_0000 | 0x0010_0000;
  const ESCAPE: u16 = 0x35;

  #[link(name = "CoreGraphics", kind = "framework")]
  unsafe extern "C" {
    fn CGEventSourceFlagsState(state_id: i32) -> u64;
    fn CGEventSourceKeyState(state_id: i32, key: u16) -> bool;
    fn CGEventSourceButtonState(state_id: i32, button: u32) -> bool;
  }

  pub fn modifiers_held() -> bool {
    unsafe { CGEventSourceFlagsState(HID_SYSTEM_STATE) & MODIFIERS != 0 }
  }

  pub fn interrupted() -> bool {
    unsafe {
      CGEventSourceKeyState(HID_SYSTEM_STATE, ESCAPE)
        || CGEventSourceButtonState(HID_SYSTEM_STATE, 0)
        || CGEventSourceButtonState(HID_SYSTEM_STATE, 1)
    }
  }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod keys {
  pub fn modifiers_held() -> bool {
    false
  }

  pub fn interrupted() -> bool {
    false
  }
}

/// An input method (the Vietnamese Telex or VNI of macOS) rewrites typed
/// letters. Typing goes through the ASCII keyboard layout instead, and the
/// person's input source comes back afterwards. The calls run on the main
/// thread, as the Text Input Sources API requires.
#[cfg(target_os = "macos")]
mod input_source {
  use std::ffi::c_void;
  use tauri::AppHandle;

  #[link(name = "Carbon", kind = "framework")]
  unsafe extern "C" {
    fn TISCopyCurrentKeyboardInputSource() -> *const c_void;
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *const c_void;
    fn TISSelectInputSource(source: *const c_void) -> i32;
  }

  #[link(name = "CoreFoundation", kind = "framework")]
  unsafe extern "C" {
    fn CFEqual(a: *const c_void, b: *const c_void) -> u8;
    fn CFRelease(cf: *const c_void);
  }

  fn on_main<T: Send + 'static>(
    app: &AppHandle,
    f: impl FnOnce() -> T + Send + 'static,
  ) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    app
      .run_on_main_thread(move || {
        let _ = tx.send(f());
      })
      .ok()?;
    rx.recv_timeout(std::time::Duration::from_secs(2)).ok()
  }

  /// Switches to the ASCII layout when another source is selected; returns
  /// the source to switch back to.
  pub fn use_ascii(app: &AppHandle) -> Option<usize> {
    on_main(app, || unsafe {
      let current = TISCopyCurrentKeyboardInputSource();
      let ascii = TISCopyCurrentASCIICapableKeyboardLayoutInputSource();
      if current.is_null() || ascii.is_null() || CFEqual(current, ascii) != 0 {
        if !current.is_null() {
          CFRelease(current);
        }
        if !ascii.is_null() {
          CFRelease(ascii);
        }
        return None;
      }
      TISSelectInputSource(ascii);
      CFRelease(ascii);
      Some(current as usize)
    })
    .flatten()
  }

  pub fn restore(app: &AppHandle, previous: usize) {
    on_main(app, move || unsafe {
      let previous = previous as *const c_void;
      TISSelectInputSource(previous);
      CFRelease(previous);
    });
  }
}

fn dwell(rng: &mut impl rand::Rng) -> Duration {
  Duration::from_secs_f64(normal(rng, DWELL_MEAN_MS, DWELL_STD_MS).clamp(40.0, 160.0) / 1000.0)
}

/// Waits until `until`, or returns false when the person stopped it.
fn wait_until(started: Instant, until: Duration) -> bool {
  loop {
    if keys::interrupted() {
      return false;
    }
    let now = started.elapsed();
    if now >= until {
      return true;
    }
    std::thread::sleep((until - now).min(Duration::from_millis(15)));
  }
}

fn press(
  enigo: &mut enigo::Enigo,
  stroke: Stroke,
  hold: Duration,
  rng: &mut impl rand::Rng,
) -> Result<(), enigo::InputError> {
  use enigo::{Direction, Key, Keyboard};
  let (key, shift) = match stroke {
    Stroke::Backspace => (Key::Backspace, false),
    Stroke::Char(c) => match us_key(c) {
      Some((base, shift)) => (os_key(base), shift),
      None => return enigo.text(&c.to_string()),
    },
  };
  if shift {
    enigo.key(Key::Shift, Direction::Press)?;
    std::thread::sleep(Duration::from_millis(rng.random_range(25..60)));
  }
  enigo.key(key, Direction::Press)?;
  std::thread::sleep(hold);
  let released = enigo.key(key, Direction::Release);
  if shift {
    std::thread::sleep(Duration::from_millis(rng.random_range(10..40)));
    enigo.key(Key::Shift, Direction::Release)?;
  }
  released
}

/// Types `text` into the focused field. `Ok(true)` when it typed all of it,
/// `Ok(false)` when Escape or a click stopped it.
fn type_text(app: &AppHandle, text: &str) -> Result<bool, String> {
  let held_since = Instant::now();
  while keys::modifiers_held() {
    if held_since.elapsed() > KEYS_HELD_LIMIT {
      return Err(crate::backend_error("PASTE_TYPING_KEYS_HELD"));
    }
    std::thread::sleep(Duration::from_millis(20));
  }
  let mut enigo = enigo::Enigo::new(&enigo::Settings::default()).map_err(|e| match e {
    enigo::NewConError::NoPermission => crate::backend_error("PASTE_TYPING_PERMISSION"),
    other => crate::backend_error_with_detail("PASTE_TYPING_FAILED", other.to_string()),
  })?;
  let strokes = plan(text);

  #[cfg(target_os = "macos")]
  let previous_source = input_source::use_ascii(app);
  #[cfg(not(target_os = "macos"))]
  let _ = app;

  let mut rng = rand::rng();
  let started = Instant::now();
  let mut outcome = Ok(true);
  for (i, planned) in strokes.iter().enumerate() {
    if !wait_until(started, planned.at) {
      outcome = Ok(false);
      break;
    }
    // Let go before the next key comes down, as fast typists do not always.
    let gap = strokes
      .get(i + 1)
      .map_or(Duration::from_millis(200), |next| {
        next.at.saturating_sub(planned.at)
      });
    let hold = dwell(&mut rng).min(gap.mul_f64(0.7).max(Duration::from_millis(30)));
    if let Err(e) = press(&mut enigo, planned.stroke, hold, &mut rng) {
      outcome = Err(crate::backend_error_with_detail(
        "PASTE_TYPING_FAILED",
        e.to_string(),
      ));
      break;
    }
  }

  #[cfg(target_os = "macos")]
  if let Some(previous) = previous_source {
    input_source::restore(app, previous);
  }
  outcome
}

fn run(app: AppHandle) {
  let started = Instant::now();
  let result = {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    app
      .clipboard()
      .read_text()
      .map_err(|_| crate::backend_error("PASTE_TYPING_EMPTY"))
      .and_then(|text| sanitize(&text))
      .and_then(|text| type_text(&app, &text).map(|done| (done, text.chars().count())))
  };
  match result {
    Ok((true, chars)) => log::info!(
      "Paste typed chars={chars} elapsed_ms={}",
      started.elapsed().as_millis()
    ),
    Ok((false, chars)) => log::info!(
      "Paste typing stopped by the person chars={chars} elapsed_ms={}",
      started.elapsed().as_millis()
    ),
    Err(error) => {
      log::warn!("Paste typing failed err={error}");
      let _ = crate::events::emit("paste-typing-failed", serde_json::json!({ "error": error }));
    }
  }
  TYPING.store(false, Ordering::SeqCst);
}

/// The shortcut: Ctrl+Alt+V, or ⌘⌥V on macOS.
pub fn shortcut() -> tauri_plugin_global_shortcut::Shortcut {
  use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};
  #[cfg(target_os = "macos")]
  let primary = Modifiers::SUPER;
  #[cfg(not(target_os = "macos"))]
  let primary = Modifiers::CONTROL;
  Shortcut::new(Some(primary | Modifiers::ALT), Code::KeyV)
}

/// The global-shortcut plugin's handler.
pub fn on_shortcut(
  app: &AppHandle,
  pressed: &tauri_plugin_global_shortcut::Shortcut,
  event: tauri_plugin_global_shortcut::ShortcutEvent,
) {
  if *pressed != shortcut() || event.state() != tauri_plugin_global_shortcut::ShortcutState::Pressed
  {
    return;
  }
  if TYPING.swap(true, Ordering::SeqCst) {
    return;
  }
  let app = app.clone();
  std::thread::spawn(move || run(app));
}

/// Takes or gives up the shortcut, following the setting.
pub fn apply(app: &AppHandle, enabled: bool) {
  use tauri_plugin_global_shortcut::GlobalShortcutExt;
  let shortcuts = app.global_shortcut();
  let registered = shortcuts.is_registered(shortcut());
  let result = match (enabled, registered) {
    (true, false) => shortcuts.register(shortcut()),
    (false, true) => shortcuts.unregister(shortcut()),
    _ => return,
  };
  match result {
    Ok(()) => log::info!("Paste typing shortcut enabled={enabled}"),
    Err(e) => log::warn!("Paste typing shortcut not changed enabled={enabled} err=\"{e}\""),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_plan_always_ends_on_exactly_the_copied_text() {
    for text in [
      "4111 1111 1111 1111",
      "4111111111111111",
      "1234 Main Street, Apt 5B",
      "John O'Neil",
      "Portland, OR 97201",
      "name+tag@example.com",
    ] {
      for _ in 0..50 {
        let strokes = plan(text);
        assert_eq!(replay(strokes.iter().map(|s| s.stroke)), text);
        assert!(strokes.windows(2).all(|w| w[0].at <= w[1].at));
      }
    }
  }

  #[test]
  fn a_long_number_pauses_between_groups_of_four() {
    let strokes = plan("4111111111111111");
    let total = strokes.last().unwrap().at;
    // 16 digits at 45 WPM is about 4.3 s of keys; three group pauses add more.
    assert!(total > Duration::from_secs_f64(3.0), "{total:?}");
  }

  #[test]
  fn the_clipboard_becomes_one_line_and_too_much_is_refused() {
    assert_eq!(sanitize("  4111 1111\r\n").unwrap(), "4111 1111");
    assert_eq!(
      sanitize("123 Main St\nPortland\tOR").unwrap(),
      "123 Main St Portland OR"
    );
    assert!(sanitize(" \n\t ")
      .unwrap_err()
      .contains("PASTE_TYPING_EMPTY"));
    let long = "a".repeat(MAX_CHARS + 1);
    let refused: serde_json::Value = serde_json::from_str(&sanitize(&long).unwrap_err()).unwrap();
    assert_eq!(refused["code"], "PASTE_TYPING_TOO_LONG");
    assert_eq!(refused["params"]["max"], MAX_CHARS.to_string());
  }

  #[test]
  fn capitals_and_symbols_are_typed_with_shift_on_their_own_key() {
    assert_eq!(us_key('a'), Some(('a', false)));
    assert_eq!(us_key('Q'), Some(('q', true)));
    assert_eq!(us_key('7'), Some(('7', false)));
    assert_eq!(us_key('@'), Some(('2', true)));
    assert_eq!(us_key('"'), Some(('\'', true)));
    assert_eq!(us_key('/'), Some(('/', false)));
    assert_eq!(us_key(' '), Some((' ', false)));
    assert_eq!(us_key('é'), None);
    assert_eq!(us_key('ư'), None);
  }
}
