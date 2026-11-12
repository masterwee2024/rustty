//! Translation of egui keyboard events into xterm-style terminal input.

use egui::{Event, Key, Modifiers};

/// Encode a single egui event as bytes to send to the remote pty.
///
/// `app_cursor` should be true when the remote application enabled
/// DECCKM (application cursor keys).
pub fn encode_event(event: &Event, app_cursor: bool, current: Modifiers) -> Option<Vec<u8>> {
    match event {
        // egui-winit turns Ctrl+C / Ctrl+X into clipboard events and does not
        // emit a key event. A terminal still needs the control bytes.
        Event::Copy => Some(vec![0x03]),
        Event::Cut => Some(vec![0x18]),
        Event::Text(text) => {
            // Control chords are handled as key events so that egui's synthesized
            // text does not produce a duplicate byte.
            if (current.ctrl && !current.alt) || text.is_empty() {
                None
            } else {
                Some(text.as_bytes().to_vec())
            }
        }
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => encode_key(*key, *modifiers, app_cursor),
        _ => None,
    }
}

fn encode_key(key: Key, mods: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    // AltGr arrives as ctrl+alt on Windows and must not be treated as Ctrl.
    let ctrl = mods.ctrl && !mods.alt;

    if ctrl {
        if let Some(byte) = control_byte(key) {
            return Some(vec![byte]);
        }
    }

    let modifier = 1 + u8::from(mods.shift) + 2 * u8::from(mods.alt) + 4 * u8::from(mods.ctrl);

    let modified = modifier > 1;

    let arrow = |letter: char| -> Vec<u8> {
        if !modified && !app_cursor {
            format!("\x1b[{letter}").into_bytes()
        } else if !modified {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[1;{modifier}{letter}").into_bytes()
        }
    };

    match key {
        Key::Enter => Some(if mods.alt {
            b"\x1b\r".to_vec()
        } else {
            b"\r".to_vec()
        }),
        Key::Backspace => Some(if ctrl {
            vec![0x08]
        } else if mods.alt {
            b"\x1b\x7f".to_vec()
        } else {
            vec![0x7f]
        }),
        Key::Tab => Some(if mods.shift {
            b"\x1b[Z".to_vec()
        } else if mods.alt {
            b"\x1b\t".to_vec()
        } else {
            b"\t".to_vec()
        }),
        Key::Escape => Some(b"\x1b".to_vec()),
        Key::ArrowUp => Some(arrow('A')),
        Key::ArrowDown => Some(arrow('B')),
        Key::ArrowRight => Some(arrow('C')),
        Key::ArrowLeft => Some(arrow('D')),
        Key::Home => Some(if modified {
            format!("\x1b[1;{modifier}H").into_bytes()
        } else {
            b"\x1b[H".to_vec()
        }),
        Key::End => Some(if modified {
            format!("\x1b[1;{modifier}F").into_bytes()
        } else {
            b"\x1b[F".to_vec()
        }),
        Key::Insert => Some(tilde_key(2, modifier)),
        Key::Delete => Some(tilde_key(3, modifier)),
        Key::PageUp => Some(tilde_key(5, modifier)),
        Key::PageDown => Some(tilde_key(6, modifier)),
        Key::F1 => Some(function_key(1, modifier)),
        Key::F2 => Some(function_key(2, modifier)),
        Key::F3 => Some(function_key(3, modifier)),
        Key::F4 => Some(function_key(4, modifier)),
        Key::F5 => Some(tilde_key(15, modifier)),
        Key::F6 => Some(tilde_key(17, modifier)),
        Key::F7 => Some(tilde_key(18, modifier)),
        Key::F8 => Some(tilde_key(19, modifier)),
        Key::F9 => Some(tilde_key(20, modifier)),
        Key::F10 => Some(tilde_key(21, modifier)),
        Key::F11 => Some(tilde_key(23, modifier)),
        Key::F12 => Some(tilde_key(24, modifier)),
        Key::Space if mods.alt => Some(b"\x1b ".to_vec()),
        // Printable keys are delivered through Event::Text. Anything left here
        // (e.g. shifted punctuation) is intentionally ignored to avoid doubles.
        _ => None,
    }
}

fn tilde_key(number: u8, modifier: u8) -> Vec<u8> {
    if modifier == 1 {
        format!("\x1b[{number}~").into_bytes()
    } else {
        format!("\x1b[{number};{modifier}~").into_bytes()
    }
}

fn function_key(number: u8, modifier: u8) -> Vec<u8> {
    let letter = match number {
        1 => 'P',
        2 => 'Q',
        3 => 'R',
        _ => 'S',
    };
    if modifier == 1 {
        format!("\x1bO{letter}").into_bytes()
    } else {
        format!("\x1b[1;{modifier}{letter}").into_bytes()
    }
}

fn control_byte(key: Key) -> Option<u8> {
    Some(match key {
        Key::A => 0x01,
        Key::B => 0x02,
        Key::C => 0x03,
        Key::D => 0x04,
        Key::E => 0x05,
        Key::F => 0x06,
        Key::G => 0x07,
        Key::H => 0x08,
        Key::I => 0x09,
        Key::J => 0x0a,
        Key::K => 0x0b,
        Key::L => 0x0c,
        Key::M => 0x0d,
        Key::N => 0x0e,
        Key::O => 0x0f,
        Key::P => 0x10,
        Key::Q => 0x11,
        Key::R => 0x12,
        Key::S => 0x13,
        Key::T => 0x14,
        Key::U => 0x15,
        Key::V => 0x16,
        Key::W => 0x17,
        Key::X => 0x18,
        Key::Y => 0x19,
        Key::Z => 0x1a,
        Key::Space => 0x00,
        Key::OpenBracket => 0x1b,
        Key::Backslash => 0x1c,
        Key::CloseBracket => 0x1d,
        Key::Backtick => 0x00,
        Key::Minus => 0x1f,
        Key::Num2 => 0x00,
        Key::Num3 => 0x1b,
        Key::Num4 => 0x1c,
        Key::Num5 => 0x1d,
        Key::Num6 => 0x1e,
        Key::Num7 => 0x1f,
        Key::Num8 => 0x7f,
        _ => return None,
    })
}

/// Build a paste payload, honoring bracketed-paste mode.
pub fn paste_payload(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if bracketed {
        let mut out = Vec::with_capacity(normalized.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(normalized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        normalized.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, modifiers: Modifiers) -> Vec<u8> {
        encode_key(key, modifiers, false).unwrap()
    }

    #[test]
    fn control_letters() {
        assert_eq!(key(Key::C, Modifiers::CTRL), vec![0x03]);
        assert_eq!(key(Key::D, Modifiers::CTRL), vec![0x04]);
        assert_eq!(key(Key::Space, Modifiers::CTRL), vec![0x00]);
    }

    #[test]
    fn arrows_and_modifiers() {
        assert_eq!(key(Key::ArrowUp, Modifiers::NONE), b"\x1b[A".to_vec());
        assert_eq!(key(Key::ArrowRight, Modifiers::CTRL), b"\x1b[1;5C".to_vec());
        assert_eq!(key(Key::ArrowLeft, Modifiers::ALT), b"\x1b[1;3D".to_vec());
        assert_eq!(
            encode_key(Key::ArrowUp, Modifiers::NONE, true).unwrap(),
            b"\x1bOA".to_vec()
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(key(Key::F1, Modifiers::NONE), b"\x1bOP".to_vec());
        assert_eq!(key(Key::F5, Modifiers::NONE), b"\x1b[15~".to_vec());
        assert_eq!(key(Key::F6, Modifiers::SHIFT), b"\x1b[17;2~".to_vec());
    }

    #[test]
    fn enter_and_backspace() {
        assert_eq!(key(Key::Enter, Modifiers::NONE), b"\r".to_vec());
        assert_eq!(key(Key::Backspace, Modifiers::NONE), vec![0x7f]);
        assert_eq!(key(Key::Tab, Modifiers::SHIFT), b"\x1b[Z".to_vec());
    }

    #[test]
    fn clipboard_events_become_control_keys() {
        assert_eq!(
            encode_event(&Event::Copy, false, Modifiers::CTRL).unwrap(),
            vec![0x03]
        );
        assert_eq!(
            encode_event(&Event::Cut, false, Modifiers::CTRL).unwrap(),
            vec![0x18]
        );
    }

    #[test]
    fn bracketed_paste() {
        assert_eq!(
            paste_payload("a\nb", true),
            b"\x1b[200~a\nb\x1b[201~".to_vec()
        );
    }
}
