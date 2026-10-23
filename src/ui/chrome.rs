use super::*;
use gpui_kit::component::TitleBar;

pub(super) fn title_bar(cx: &App) -> impl IntoElement {
    let p = palette(cx);
    TitleBar::new()
        .bg(rgb(p.bg))
        .border_color(rgb(p.line))
        .text_color(rgb(p.text))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(img("brand/logo.png").size(px(20.)))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .child("OCI Pull"),
                ),
        )
}

pub(super) fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("OCI Pull".into()),
            ..TitleBar::title_bar_options()
        }),
        ..TitleBar::window_options()
    }
}
