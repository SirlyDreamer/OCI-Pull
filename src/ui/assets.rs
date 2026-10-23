use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

gpui_kit::assets::icon_assets!(
    TaskIcons,
    [Play, Pause, Trash, Container, SlidersHorizontal]
);

pub(super) struct AppAssets;
impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == "brand/logo.png" {
            return Ok(Some(Cow::Borrowed(include_bytes!(
                "../../assets/brand/logo.png"
            ))));
        }
        if let Some(bytes) = TaskIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }
    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        if "brand/logo.png".starts_with(path) {
            paths.push("brand/logo.png".into());
        }
        paths.extend(TaskIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::{AppAssets, AssetSource};
    #[test]
    fn lucide_controls_and_default_component_icons_are_embedded() {
        for path in [
            "icons/play.svg",
            "icons/pause.svg",
            "icons/trash.svg",
            "icons/container.svg",
            "icons/sliders-horizontal.svg",
            "icons/search.svg",
            "icons/folder.svg",
            "icons/window-minimize.svg",
            "icons/window-maximize.svg",
            "icons/window-restore.svg",
            "icons/window-close.svg",
        ] {
            let bytes = AppAssets.load(path).unwrap().unwrap();
            assert!(std::str::from_utf8(&bytes).unwrap().contains("<svg"));
        }
    }
}
