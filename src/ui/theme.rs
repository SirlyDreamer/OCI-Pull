use gpui_kit::{
    component::{Theme, ThemeMode},
    *,
};

#[cfg(target_os = "windows")]
pub(super) const FONT: &str = "Segoe UI";
#[cfg(target_os = "macos")]
pub(super) const FONT: &str = "Helvetica Neue";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(super) const FONT: &str = "DejaVu Sans";

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub bg: u32,
    pub panel: u32,
    pub subtle: u32,
    pub hover: u32,
    pub line: u32,
    pub text: u32,
    pub muted: u32,
    pub accent: u32,
    pub on_accent: u32,
    pub green: u32,
    pub red: u32,
    pub error_bg: u32,
}
impl Palette {
    pub fn for_mode(mode: ThemeMode) -> Self {
        if mode.is_dark() {
            Self {
                bg: 0x0A0A0A,
                panel: 0x0A0A0A,
                subtle: 0x171717,
                hover: 0x262626,
                line: 0x303030,
                text: 0xFAFAFA,
                muted: 0xA3A3A3,
                accent: 0xE5E5E5,
                on_accent: 0x171717,
                green: 0x86EFAC,
                red: 0xFCA5A5,
                error_bg: 0x311A1A,
            }
        } else {
            Self {
                bg: 0xFFFFFF,
                panel: 0xFFFFFF,
                subtle: 0xFAFAFA,
                hover: 0xF5F5F5,
                line: 0xE5E5E5,
                text: 0x0A0A0A,
                muted: 0x737373,
                accent: 0x171717,
                on_accent: 0xFAFAFA,
                green: 0x15803D,
                red: 0xB91C1C,
                error_bg: 0xFEF2F2,
            }
        }
    }
}
pub(super) fn palette(cx: &App) -> Palette {
    Palette::for_mode(Theme::global(cx).mode)
}

// Both native controls and custom surfaces use the reference's neutral palette.
pub(super) fn apply(mode: ThemeMode, cx: &mut App) {
    Theme::change(mode, None, cx);
    let p = Palette::for_mode(mode);
    let dark = mode.is_dark();
    let t = Theme::global_mut(cx);
    t.font_family = FONT.into();
    t.radius = px(8.);
    t.radius_lg = px(14.);
    t.background = rgb(p.bg).into();
    t.foreground = rgb(p.text).into();
    t.border = rgb(p.line).into();
    t.input = rgb(p.line).into();
    t.ring = rgb(if dark { 0x737373 } else { 0xA3A3A3 }).into();
    t.caret = rgb(p.text).into();
    t.accent = rgb(p.hover).into();
    t.accent_foreground = rgb(p.text).into();
    t.muted = rgb(p.hover).into();
    t.muted_foreground = rgb(p.muted).into();
    t.primary = rgb(p.accent).into();
    t.primary_hover = rgb(if dark { 0xD4D4D4 } else { 0x404040 }).into();
    t.primary_active = rgb(if dark { 0xBDBDBD } else { 0x0A0A0A }).into();
    t.primary_foreground = rgb(p.on_accent).into();
    t.button_primary = t.primary;
    t.button_primary_hover = t.primary_hover;
    t.button_primary_active = t.primary_active;
    t.button_primary_foreground = t.primary_foreground;
    t.button = rgb(p.subtle).into();
    t.button_foreground = rgb(p.text).into();
    t.button_hover = rgb(p.hover).into();
    t.button_active = rgb(p.line).into();
    t.secondary = rgb(p.hover).into();
    t.secondary_foreground = rgb(p.text).into();
    t.secondary_hover = rgb(p.line).into();
    t.secondary_active = rgb(if dark { 0x404040 } else { 0xD4D4D4 }).into();
    t.button_secondary = t.secondary;
    t.button_secondary_foreground = t.secondary_foreground;
    t.button_secondary_hover = t.secondary_hover;
    t.button_secondary_active = t.secondary_active;
    t.popover = rgb(if dark { p.subtle } else { p.bg }).into();
    t.popover_foreground = rgb(p.text).into();
    t.colors.list = t.popover;
    t.list_hover = rgb(p.hover).into();
    t.list_active = rgb(p.hover).into();
    t.list_active_border = rgb(p.line).into();
    t.list_head = rgb(p.subtle).into();
    t.list_even = t.popover;
    t.selection = rgb(if dark { 0x525252 } else { 0xD4D4D4 }).into();
    t.progress_bar = rgb(p.accent).into();
    t.scrollbar = transparent_black();
    t.scrollbar_thumb = rgb(if dark { 0x525252 } else { 0xD4D4D4 }).into();
    t.scrollbar_thumb_hover = rgb(p.muted).into();
    t.overlay = rgba(if dark { 0x00000080 } else { 0x0000001A }).into();
    t.title_bar = rgb(p.bg).into();
    t.title_bar_border = rgb(p.line).into();
    t.tokens = t.colors.into();
    Theme::sync_base(cx);
}

#[cfg(test)]
mod tests {
    use super::Palette;
    use gpui_kit::component::ThemeMode;
    fn luminance(color: u32) -> f64 {
        let channel = |shift: u32| {
            let c = ((color >> shift) & 255u32) as f64 / 255.;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }
    fn contrast(a: u32, b: u32) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
    #[test]
    fn light_and_dark_text_remain_readable() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let p = Palette::for_mode(mode);
            for surface in [p.bg, p.subtle, p.hover] {
                assert!(contrast(p.text, surface) >= 4.5);
                assert!(contrast(p.muted, surface) >= 4.0);
            }
            assert!(contrast(p.accent, p.on_accent) >= 4.5);
            assert!(contrast(p.red, p.error_bg) >= 4.5);
        }
    }
}
