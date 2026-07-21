const INJECT_KEYCODE: u8 = 0;
const INJECT_TEXT: u8 = 1;
const INJECT_TOUCH: u8 = 2;
const BACK_OR_SCREEN_ON: u8 = 4;
const SET_SCREEN_POWER_MODE: u8 = 10;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum TouchAction {
    Down = 0,
    Up = 1,
    Move = 2,
}

pub fn keycode(code: u32, action: u8, repeat: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(14);
    out.extend([INJECT_KEYCODE, action]);
    out.extend(code.to_be_bytes());
    out.extend(repeat.to_be_bytes());
    out.extend(0u32.to_be_bytes());
    out
}

pub fn text(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(5 + bytes.len());
    out.push(INJECT_TEXT);
    out.extend((bytes.len() as u32).to_be_bytes());
    out.extend(bytes);
    out
}

pub fn touch(action: TouchAction, x: i32, y: i32, width: u16, height: u16) -> Vec<u8> {
    // scrcpy's reserved virtual-finger id (UINT64_MAX - 2).
    touch_pointer(action, x, y, width, height, u64::MAX - 2)
}
pub fn touch_pointer(
    action: TouchAction,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
    pointer_id: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend([INJECT_TOUCH, action as u8]);
    out.extend(pointer_id.to_be_bytes());
    out.extend(x.to_be_bytes());
    out.extend(y.to_be_bytes());
    out.extend(width.to_be_bytes());
    out.extend(height.to_be_bytes());
    let pressure: u16 = if matches!(action, TouchAction::Up) {
        0
    } else {
        0xffff
    };
    out.extend(pressure.to_be_bytes());
    out.extend(0u32.to_be_bytes());
    out.extend(0u32.to_be_bytes());
    out
}

pub fn back_or_screen_on(action: u8) -> [u8; 2] {
    [BACK_OR_SCREEN_ON, action]
}
pub fn screen_power(mode: u8) -> [u8; 2] {
    [SET_SCREEN_POWER_MODE, mode]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_lengths_match_v4() {
        assert_eq!(keycode(4, 0, 0).len(), 14);
        assert_eq!(touch(TouchAction::Down, 1, 2, 1080, 2400).len(), 32);
        assert_eq!(text("中").len(), 8);
    }
}
