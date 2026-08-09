//! Host keyboard layout, followed through KDE's D-Bus interface.
//!
//! Universal Symbols resolve their keycodes against the layout the firmware
//! believes is active, so a missed update here means the keyboard types the
//! wrong characters — this is a correctness path, not a cosmetic one.

use std::time::Duration;

use anyhow::{Context, Result};

/// Layout codes the firmware understands (`universal_symbols::HostLayout`).
const LAYOUT_EN: u8 = 0;
const LAYOUT_RU: u8 = 1;

/// D-Bus has no protocol-level reply timeout and zbus applies none by default,
/// so a name that is owned by a wedged process (kwin stuck on a GPU reset) would
/// hang every call forever — and these calls are awaited inside the event loop.
const METHOD_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Connects to the layout service, bounded in time.
    ///
    /// The probe that calls this runs inside the event loop, so an unbounded
    /// connect would stall the heartbeat and the clock along with it — and the
    /// handshake happens before `method_timeout` can apply to anything.
    pub async fn connect() -> Result<Self> {
        tokio::time::timeout(METHOD_TIMEOUT, Self::connect_inner())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "the layout service did not answer within {}s",
                    METHOD_TIMEOUT.as_secs()
                )
            })?
    }

    async fn connect_inner() -> Result<Self> {
        let connection = zbus::connection::Builder::session()
            .context("locating the session bus")?
            .method_timeout(METHOD_TIMEOUT)
            .build()
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

    pub async fn refresh_names(&mut self) {
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

    /// Signal streams for layout changes and for edits to the layout list.
    ///
    /// Built from cloned proxies so the streams do not borrow `self` — the main
    /// loop needs `&mut self` to resolve indices while holding them.
    pub async fn signals(&self) -> Result<Signals> {
        Ok(Signals {
            changed: self
                .proxy
                .clone()
                .receive_layout_changed()
                .await
                .context("subscribing to layoutChanged")?,
            list_changed: self
                .proxy
                .clone()
                .receive_layout_list_changed()
                .await
                .context("subscribing to layoutListChanged")?,
        })
    }
}

/// What the layout service just told us.
pub enum Change {
    /// The active layout moved to this index.
    Active(u32),
    /// The list itself was edited, so the cached names are suspect.
    ListEdited,
    /// The stream ended: the bus connection behind it is gone for good and no
    /// further signal will ever arrive on it.
    Lost,
    /// A signal we could not read. Nothing to do but wait for the next one.
    Unreadable,
}

pub struct Signals {
    changed: layoutChangedStream,
    list_changed: layoutListChangedStream,
}

impl Signals {
    /// Waits for the next layout signal.
    ///
    /// Both arms are stream reads, which are cancellation-safe, so this is safe
    /// to use as a `select!` arm in the main loop.
    pub async fn next(&mut self) -> Change {
        use futures_util::StreamExt as _;

        tokio::select! {
            signal = self.changed.next() => match signal {
                None => Change::Lost,
                Some(signal) => match signal.args() {
                    Ok(args) => Change::Active(args.index),
                    Err(err) => {
                        log::warn!("cannot read a layoutChanged signal: {err}");
                        Change::Unreadable
                    }
                },
            },
            signal = self.list_changed.next() => match signal {
                None => Change::Lost,
                Some(_) => Change::ListEdited,
            },
        }
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
