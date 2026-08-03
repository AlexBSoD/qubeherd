//! Host keyboard layout, followed through KDE's D-Bus interface.
//!
//! Universal Symbols resolve their keycodes against the layout the firmware
//! believes is active, so a missed update here means the keyboard types the
//! wrong characters — this is a correctness path, not a cosmetic one.

use anyhow::{Context, Result};
use zbus::Connection;

/// Layout codes the firmware understands (`universal_symbols::HostLayout`).
const LAYOUT_EN: u8 = 0;
const LAYOUT_RU: u8 = 1;

pub fn code_name(code: u8) -> &'static str {
    if code == LAYOUT_RU {
        "ru"
    } else {
        "en"
    }
}

#[zbus::proxy(
    interface = "org.kde.KeyboardLayouts",
    default_service = "org.kde.keyboard",
    default_path = "/Layouts"
)]
pub trait KeyboardLayouts {
    #[zbus(name = "getLayout")]
    fn get_layout(&self) -> zbus::Result<u32>;

    #[zbus(name = "getLayoutsList")]
    fn get_layouts_list(&self) -> zbus::Result<Vec<(String, String, String)>>;

    #[zbus(signal, name = "layoutChanged")]
    fn layout_changed(&self, index: u32) -> zbus::Result<()>;

    #[zbus(signal, name = "layoutListChanged")]
    fn layout_list_changed(&self) -> zbus::Result<()>;
}

pub struct KdeLayouts {
    proxy: KeyboardLayoutsProxy<'static>,
    names: Vec<String>,
}

impl KdeLayouts {
    pub async fn connect() -> Result<Self> {
        let connection = Connection::session()
            .await
            .context("connecting to the session bus")?;
        let proxy = KeyboardLayoutsProxy::new(&connection)
            .await
            .context("building the org.kde.keyboard proxy")?;
        // Fail here rather than at the first layout change, so a non-KDE
        // session degrades at startup with one clear warning.
        proxy
            .get_layout()
            .await
            .context("org.kde.keyboard is not answering")?;
        let mut layouts = Self {
            proxy,
            names: Vec::new(),
        };
        layouts.refresh_names().await;
        Ok(layouts)
    }

    async fn refresh_names(&mut self) {
        match self.proxy.get_layouts_list().await {
            Ok(list) => {
                self.names = list
                    .into_iter()
                    .map(|(short_name, _, _)| short_name)
                    .collect()
            }
            Err(err) => log::warn!("cannot read the layout list: {err}"),
        }
    }

    /// Resolves a KDE layout index to a firmware code.
    ///
    /// Returns `None` for layouts the firmware has no mapping for (a third
    /// layout such as `de`), which leaves the last known one in place.
    pub async fn code_for_index(&mut self, index: u32) -> Option<u8> {
        if index as usize >= self.names.len() {
            self.refresh_names().await;
        }
        let name = self.names.get(index as usize)?;
        normalize(name)
    }

    pub async fn current(&mut self) -> Option<u8> {
        let index = self
            .proxy
            .get_layout()
            .await
            .map_err(|err| log::warn!("cannot read the active layout: {err}"))
            .ok()?;
        self.code_for_index(index).await
    }

    /// Signal stream of layout changes.
    ///
    /// Built from a cloned proxy so the stream does not borrow `self` — the
    /// main loop needs `&mut self` to resolve indices while holding it.
    pub async fn changes(&self) -> Result<layoutChangedStream> {
        self.proxy
            .clone()
            .receive_layout_changed()
            .await
            .context("subscribing to layoutChanged")
    }
}

/// Maps an xkb layout name onto the firmware's layout code.
fn normalize(xkb_name: &str) -> Option<u8> {
    let name = xkb_name
        .trim()
        .split(['-', '_', '.', ':', '(', '@'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match name.as_str() {
        "en" | "us" | "gb" | "uk" | "au" | "ca" => Some(LAYOUT_EN),
        "ru" => Some(LAYOUT_RU),
        other if other.starts_with("russian") => Some(LAYOUT_RU),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_the_layout_names_kde_actually_reports() {
        // getLayoutsList yields xkb short names: ("us", "", "English (US)").
        assert_eq!(normalize("us"), Some(LAYOUT_EN));
        assert_eq!(normalize("ru"), Some(LAYOUT_RU));
        assert_eq!(normalize("en-US"), Some(LAYOUT_EN));
        assert_eq!(normalize("russianwin"), Some(LAYOUT_RU));
        assert_eq!(normalize("de"), None);
    }
}
