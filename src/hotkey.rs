//! Hotkey strings like "Ctrl+Alt+R", "F8" or "Mouse4" (mouse buttons only work for push-to-talk,
//! which is polled; Windows' RegisterHotKey can't take them).

use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub fn parse(s: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = MOD_NOREPEAT;
    let mut vk = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_uppercase()) {
        match part.as_str() {
            "" => {}
            "CTRL" | "CONTROL" => mods |= MOD_CONTROL,
            "ALT" => mods |= MOD_ALT,
            "SHIFT" => mods |= MOD_SHIFT,
            "WIN" => mods |= MOD_WIN,
            k if k.len() > 1 && k.starts_with('F') && k[1..].chars().all(|c| c.is_ascii_digit()) => {
                let n: u32 = k[1..].parse().ok().filter(|n| (1..=24).contains(n))?;
                vk = Some(VK_F1.0 as u32 + n - 1);
            }
            k if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => vk = Some(k.as_bytes()[0] as u32),
            "PRINTSCREEN" | "PRTSC" => vk = Some(VK_SNAPSHOT.0 as u32),
            "INSERT" => vk = Some(VK_INSERT.0 as u32),
            "HOME" => vk = Some(VK_HOME.0 as u32),
            "END" => vk = Some(VK_END.0 as u32),
            "PAGEUP" => vk = Some(VK_PRIOR.0 as u32),
            "PAGEDOWN" => vk = Some(VK_NEXT.0 as u32),
            "PAUSE" => vk = Some(VK_PAUSE.0 as u32),
            "CAPSLOCK" => vk = Some(VK_CAPITAL.0 as u32),
            "MOUSE4" => vk = Some(VK_XBUTTON1.0 as u32),
            "MOUSE5" => vk = Some(VK_XBUTTON2.0 as u32),
            "MIDDLEMOUSE" => vk = Some(VK_MBUTTON.0 as u32),
            k if k.starts_with("NUM") && k.len() == 4 => vk = Some(VK_NUMPAD0.0 as u32 + k[3..].parse::<u32>().ok()?),
            _ => return None,
        }
    }
    Some((mods, vk?))
}

/// Virtual-key codes of the modifiers in `mods`.
pub fn modifier_keys(mods: HOT_KEY_MODIFIERS) -> Vec<i32> {
    [(MOD_CONTROL, VK_CONTROL), (MOD_ALT, VK_MENU), (MOD_SHIFT, VK_SHIFT), (MOD_WIN, VK_LWIN)]
        .into_iter()
        .filter(|(m, _)| mods.0 & m.0 != 0)
        .map(|(_, vk)| vk.0 as i32)
        .collect()
}

pub fn is_down(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
}
