//! Reverse keymap: characters and key names → physical keys, computed from the session's keymap.
//!
//! Text is typed the way a person would type it: each character becomes the keycode that
//! produces it plus the modifier keys (Shift, AltGr) its shift level needs, so apps receive real
//! key events that match the keymap they were sent.

use std::collections::HashMap;

use smithay::input::keyboard::xkb::{self, Keycode, Keysym, keysyms};
use vway_proto::{ErrorBody, ErrorCode, err};

/// Modifier keys the reverse map knows how to press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mod {
    Shift,
    Ctrl,
    Alt,
    Super,
    AltGr,
}

impl Mod {
    fn keysym(self) -> u32 {
        match self {
            Mod::Shift => keysyms::KEY_Shift_L,
            Mod::Ctrl => keysyms::KEY_Control_L,
            Mod::Alt => keysyms::KEY_Alt_L,
            Mod::Super => keysyms::KEY_Super_L,
            Mod::AltGr => keysyms::KEY_ISO_Level3_Shift,
        }
    }

    fn parse(s: &str) -> Option<Mod> {
        Some(match s.to_ascii_lowercase().as_str() {
            "shift" => Mod::Shift,
            "ctrl" | "control" => Mod::Ctrl,
            "alt" | "option" => Mod::Alt,
            "super" | "meta" | "cmd" | "command" | "win" | "logo" => Mod::Super,
            "altgr" => Mod::AltGr,
            _ => return None,
        })
    }
}

/// Physical keys for one key press: hold `mods` in order, press and release `key`, release `mods`
/// in reverse order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stroke {
    pub mods: Vec<Keycode>,
    pub key: Keycode,
}

pub struct ReverseKeymap {
    mod_keys: HashMap<Mod, Keycode>,
    /// keysym → (keycode, level modifiers)
    by_sym: HashMap<u32, (Keycode, Vec<Mod>)>,
}

impl ReverseKeymap {
    pub fn new(layout: &str, variant: &str) -> Result<Self, String> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            "",
            "",
            layout,
            variant,
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| {
            format!("cannot compile xkb keymap layout={layout:?} variant={variant:?}")
        })?;
        Ok(Self::from_keymap(&keymap))
    }

    pub fn from_keymap(keymap: &xkb::Keymap) -> Self {
        let min = keymap.min_keycode().raw();
        let max = keymap.max_keycode().raw();
        let codes: Vec<Keycode> = (min..=max).map(Keycode::new).collect();

        let mut mod_keys = HashMap::new();
        for m in [Mod::Shift, Mod::Ctrl, Mod::Alt, Mod::Super, Mod::AltGr] {
            if let Some(kc) = codes.iter().copied().find(|kc| {
                keymap
                    .key_get_syms_by_level(*kc, 0, 0)
                    .first()
                    .is_some_and(|s| s.raw() == m.keysym())
            }) {
                mod_keys.insert(m, kc);
            }
        }

        let mask_of = |m: Mod| -> Option<u32> {
            let kc = *mod_keys.get(&m)?;
            let mut st = xkb::State::new(keymap);
            st.update_key(kc, xkb::KeyDirection::Down);
            Some(st.serialize_mods(xkb::STATE_MODS_DEPRESSED))
        };

        let mut combos: Vec<Vec<Mod>> = vec![vec![], vec![Mod::Shift]];
        if mask_of(Mod::AltGr).is_some() {
            combos.push(vec![Mod::AltGr]);
            combos.push(vec![Mod::Shift, Mod::AltGr]);
        }

        let mut by_sym = HashMap::new();
        for combo in combos {
            let Some(mask) = combo
                .iter()
                .try_fold(0u32, |acc, m| mask_of(*m).map(|mm| acc | mm))
            else {
                continue;
            };
            let mut st = xkb::State::new(keymap);
            st.update_mask(mask, 0, 0, 0, 0, 0);
            for kc in &codes {
                let sym = st.key_get_one_sym(*kc).raw();
                if sym != keysyms::KEY_NoSymbol {
                    by_sym.entry(sym).or_insert_with(|| (*kc, combo.clone()));
                }
            }
        }
        ReverseKeymap { mod_keys, by_sym }
    }

    fn mod_key(&self, m: Mod) -> Result<Keycode, ErrorBody> {
        self.mod_keys.get(&m).copied().ok_or_else(|| {
            err(
                ErrorCode::InvalidKey,
                format!("the session keymap has no {m:?} key"),
            )
        })
    }

    fn stroke_for_sym(&self, sym: u32, extra: &[Mod]) -> Option<Result<Stroke, ErrorBody>> {
        let (key, level_mods) = self.by_sym.get(&sym)?;
        let mut all: Vec<Mod> = extra.to_vec();
        for m in level_mods {
            if !all.contains(m) {
                all.push(*m);
            }
        }
        Some(
            all.iter()
                .map(|m| self.mod_key(*m))
                .collect::<Result<Vec<_>, _>>()
                .map(|mods| Stroke { mods, key: *key }),
        )
    }

    /// The stroke that types `c`, or `None` if the keymap cannot produce it.
    pub fn char_stroke(&self, c: char) -> Option<Stroke> {
        let sym = match c {
            '\n' | '\r' => keysyms::KEY_Return,
            '\t' => keysyms::KEY_Tab,
            '\u{8}' => keysyms::KEY_BackSpace,
            '\u{1b}' => keysyms::KEY_Escape,
            _ => xkb::utf32_to_keysym(c as u32).raw(),
        };
        if sym == keysyms::KEY_NoSymbol {
            return None;
        }
        self.stroke_for_sym(sym, &[]).and_then(Result::ok)
    }

    /// Strokes for a whole string; fails before anything is typed if any character is unmappable.
    pub fn text_strokes(&self, text: &str) -> Result<Vec<Stroke>, ErrorBody> {
        let mut out = Vec::with_capacity(text.len());
        let mut bad: Vec<String> = Vec::new();
        for c in text.chars() {
            match self.char_stroke(c) {
                Some(s) => out.push(s),
                None => {
                    let shown = format!("{c:?}");
                    if !bad.contains(&shown) {
                        bad.push(shown);
                    }
                }
            }
        }
        if bad.is_empty() {
            Ok(out)
        } else {
            Err(err(
                ErrorCode::UnmappableText,
                format!(
                    "the session keymap cannot type {}; nothing was typed (use --keymap to pick a \
                     layout that has these characters)",
                    bad.join(", ")
                ),
            )
            .with_details(serde_json::json!({ "characters": bad })))
        }
    }

    /// Parse a combo such as `ctrl+shift+t`, `Return`, `alt+F4`, `ctrl++`, `shift`.
    pub fn combo(&self, spec: &str) -> Result<Stroke, ErrorBody> {
        let invalid = |msg: String| err(ErrorCode::InvalidKey, msg);
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(invalid("empty key".into()));
        }
        let mut parts: Vec<&str> = if let Some(head) = spec.strip_suffix("++") {
            let mut v: Vec<&str> = if head.is_empty() {
                vec![]
            } else {
                head.split('+').collect()
            };
            v.push("+");
            v
        } else if spec == "+" {
            vec!["+"]
        } else {
            spec.split('+').collect()
        };
        if parts.iter().any(|p| p.trim().is_empty()) {
            return Err(invalid(format!("malformed key combo {spec:?}")));
        }
        let key_part = parts.pop().expect("non-empty").trim();
        let mut mods = Vec::new();
        for p in parts {
            let m = Mod::parse(p.trim())
                .ok_or_else(|| invalid(format!("{p:?} is not a modifier in {spec:?}")))?;
            if !mods.contains(&m) {
                mods.push(m);
            }
        }

        if let Some(m) = Mod::parse(key_part) {
            // A modifier pressed on its own, e.g. `shift` or `ctrl+alt`.
            let key = self.mod_key(m)?;
            let mods = mods
                .iter()
                .filter(|x| **x != m)
                .map(|x| self.mod_key(*x))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Stroke { mods, key });
        }

        let sym = key_name_to_sym(key_part)
            .ok_or_else(|| invalid(format!("unknown key {key_part:?} in {spec:?}")))?;
        match self.stroke_for_sym(sym, &mods) {
            Some(r) => r,
            None => Err(invalid(format!(
                "the session keymap has no key producing {key_part:?}"
            ))),
        }
    }

    /// Raw evdev keycode → xkb keycode.
    pub fn evdev(code: u32) -> Keycode {
        Keycode::new(code + 8)
    }
}

fn key_name_to_sym(name: &str) -> Option<u32> {
    let alias = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => Some(keysyms::KEY_Return),
        "esc" | "escape" => Some(keysyms::KEY_Escape),
        "tab" => Some(keysyms::KEY_Tab),
        "space" => Some(keysyms::KEY_space),
        "backspace" => Some(keysyms::KEY_BackSpace),
        "del" | "delete" => Some(keysyms::KEY_Delete),
        "ins" | "insert" => Some(keysyms::KEY_Insert),
        "home" => Some(keysyms::KEY_Home),
        "end" => Some(keysyms::KEY_End),
        "pgup" | "pageup" | "page_up" | "prior" => Some(keysyms::KEY_Prior),
        "pgdn" | "pagedown" | "page_down" | "next" => Some(keysyms::KEY_Next),
        "up" => Some(keysyms::KEY_Up),
        "down" => Some(keysyms::KEY_Down),
        "left" => Some(keysyms::KEY_Left),
        "right" => Some(keysyms::KEY_Right),
        "menu" => Some(keysyms::KEY_Menu),
        "print" | "printscreen" | "prtsc" => Some(keysyms::KEY_Print),
        "capslock" | "caps_lock" => Some(keysyms::KEY_Caps_Lock),
        "plus" => Some(keysyms::KEY_plus),
        "minus" => Some(keysyms::KEY_minus),
        _ => None,
    };
    if alias.is_some() {
        return alias;
    }
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let sym = xkb::utf32_to_keysym(c as u32).raw();
        return (sym != keysyms::KEY_NoSymbol).then_some(sym);
    }
    for flags in [xkb::KEYSYM_NO_FLAGS, xkb::KEYSYM_CASE_INSENSITIVE] {
        let sym: Keysym = xkb::keysym_from_name(name, flags);
        if sym.raw() != keysyms::KEY_NoSymbol {
            return Some(sym.raw());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // evdev codes + 8
    const A: u32 = 30 + 8;
    const ONE: u32 = 2 + 8;
    const T: u32 = 20 + 8;
    const LSHIFT: u32 = 42 + 8;
    const LCTRL: u32 = 29 + 8;
    const ENTER: u32 = 28 + 8;

    fn raw(s: &Stroke) -> (Vec<u32>, u32) {
        (s.mods.iter().map(|k| k.raw()).collect(), s.key.raw())
    }

    #[test]
    fn us_characters() {
        let km = ReverseKeymap::new("us", "").unwrap();
        assert_eq!(raw(&km.char_stroke('a').unwrap()), (vec![], A));
        assert_eq!(raw(&km.char_stroke('A').unwrap()), (vec![LSHIFT], A));
        assert_eq!(raw(&km.char_stroke('!').unwrap()), (vec![LSHIFT], ONE));
        assert_eq!(raw(&km.char_stroke('\n').unwrap()), (vec![], ENTER));
        assert!(km.char_stroke('ö').is_none());
        let e = km.text_strokes("hö€").unwrap_err();
        assert_eq!(e.code, ErrorCode::UnmappableText);
    }

    #[test]
    fn combos() {
        let km = ReverseKeymap::new("us", "").unwrap();
        assert_eq!(
            raw(&km.combo("ctrl+shift+t").unwrap()),
            (vec![LCTRL, LSHIFT], T)
        );
        assert_eq!(raw(&km.combo("Return").unwrap()), (vec![], ENTER));
        assert_eq!(raw(&km.combo("enter").unwrap()), (vec![], ENTER));
        assert_eq!(raw(&km.combo("shift").unwrap()), (vec![], LSHIFT));
        assert_eq!(km.combo("ctrl++").unwrap().mods.len(), 2); // ctrl + shift(=)
        assert!(km.combo("F5").is_ok());
        assert!(km.combo("XF86Back").is_ok());
        assert_eq!(
            km.combo("ctrl+nosuchkey").unwrap_err().code,
            ErrorCode::InvalidKey
        );
        assert_eq!(km.combo("hyper+a").unwrap_err().code, ErrorCode::InvalidKey);
        assert_eq!(km.combo("ctrl+").unwrap_err().code, ErrorCode::InvalidKey);
    }

    #[test]
    fn german_layout_has_umlauts_and_altgr() {
        let km = ReverseKeymap::new("de", "").unwrap();
        let o = km.char_stroke('ö').unwrap();
        assert!(o.mods.is_empty());
        let at = km.char_stroke('@').unwrap();
        assert_eq!(at.mods.len(), 1, "AltGr+q");
        assert_eq!(at.key.raw(), 16 + 8);
    }
}
