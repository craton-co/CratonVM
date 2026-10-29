// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Clipboard implementation for AWT's `java.awt.datatransfer` package.
//!
//! Manages two clipboards:
//! - **System** -- the OS clipboard (Ctrl+C / Ctrl+V).
//! - **Selection** -- X11 PRIMARY selection (middle-click paste on Linux).
//!
//! The clipboard stores data keyed by [`DataFlavor`], matching the Java
//! `DataFlavor` model.  The native platform layer is responsible for
//! synchronising with the actual OS clipboard; this module provides the
//! in-process representation.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::event::PeerId;

// ---------------------------------------------------------------------------
// DataFlavor
// ---------------------------------------------------------------------------

/// Data-transfer flavors (match `java.awt.datatransfer.DataFlavor`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DataFlavor {
    /// `text/plain; class=java.lang.String`
    StringFlavor,
    /// `text/plain; charset=unicode`
    PlainTextFlavor,
    /// `image/x-java-image; class=java.awt.Image`
    ImageFlavor,
    /// `application/x-java-file-list; class=java.util.List`
    FileListFlavor,
    /// Arbitrary MIME type.
    Custom(String),
}

// ---------------------------------------------------------------------------
// ClipboardData
// ---------------------------------------------------------------------------

/// The payload stored for a single flavor.
#[derive(Debug, Clone)]
pub enum ClipboardData {
    Text(String),
    Image {
        pixels: Vec<u32>,
        width: u32,
        height: u32,
    },
    FileList(Vec<String>),
    Raw(Vec<u8>),
}

// ---------------------------------------------------------------------------
// ClipboardContents
// ---------------------------------------------------------------------------

/// The full contents of one clipboard (possibly multi-flavor).
#[derive(Debug, Clone)]
pub struct ClipboardContents {
    data: HashMap<DataFlavor, ClipboardData>,
    owner_peer_id: Option<PeerId>,
}

impl ClipboardContents {
    /// Create empty contents.
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
            owner_peer_id: None,
        }
    }

    /// Create contents with a single text entry.
    pub fn with_text(text: String, owner: Option<PeerId>) -> Self {
        let mut data = HashMap::new();
        data.insert(DataFlavor::StringFlavor, ClipboardData::Text(text.clone()));
        data.insert(DataFlavor::PlainTextFlavor, ClipboardData::Text(text));
        Self {
            data,
            owner_peer_id: owner,
        }
    }

    /// Get the text if available (checks `StringFlavor` first, then
    /// `PlainTextFlavor`).
    pub fn get_text(&self) -> Option<&str> {
        self.data
            .get(&DataFlavor::StringFlavor)
            .or_else(|| self.data.get(&DataFlavor::PlainTextFlavor))
            .and_then(|d| match d {
                ClipboardData::Text(s) => Some(s.as_str()),
                _ => None,
            })
    }

    /// Check whether a specific flavor is present.
    pub fn has_flavor(&self, flavor: &DataFlavor) -> bool {
        self.data.contains_key(flavor)
    }

    /// List all available flavors.
    pub fn available_flavors(&self) -> Vec<DataFlavor> {
        self.data.keys().cloned().collect()
    }

    /// Get data for a specific flavor.
    pub fn get(&self, flavor: &DataFlavor) -> Option<&ClipboardData> {
        self.data.get(flavor)
    }

    /// Insert data for a flavor.
    pub fn set(&mut self, flavor: DataFlavor, data: ClipboardData) {
        self.data.insert(flavor, data);
    }

    /// Set the owning peer.
    pub fn set_owner(&mut self, owner: Option<PeerId>) {
        self.owner_peer_id = owner;
    }

    /// Get the owning peer.
    pub fn owner(&self) -> Option<PeerId> {
        self.owner_peer_id
    }
}

impl Default for ClipboardContents {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ClipboardKind
// ---------------------------------------------------------------------------

/// Which clipboard to operate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipboardKind {
    /// The system clipboard (Ctrl+C / Ctrl+V).
    System,
    /// X11 PRIMARY selection (middle-click paste).
    Selection,
}

// ---------------------------------------------------------------------------
// ClipboardManager
// ---------------------------------------------------------------------------

/// Manages system and selection clipboards.
pub struct ClipboardManager {
    system: ClipboardContents,
    selection: ClipboardContents,
}

impl ClipboardManager {
    /// Create a new clipboard manager with empty clipboards.
    pub fn new() -> Self {
        Self {
            system: ClipboardContents::new(),
            selection: ClipboardContents::new(),
        }
    }

    /// Replace the entire contents of a clipboard.
    pub fn set_contents(&mut self, which: ClipboardKind, contents: ClipboardContents) {
        match which {
            ClipboardKind::System => self.system = contents,
            ClipboardKind::Selection => self.selection = contents,
        }
    }

    /// Get a reference to a clipboard's contents.
    pub fn get_contents(&self, which: ClipboardKind) -> &ClipboardContents {
        match which {
            ClipboardKind::System => &self.system,
            ClipboardKind::Selection => &self.selection,
        }
    }

    /// Convenience: get the text from a clipboard, if any.
    pub fn get_text(&self, which: ClipboardKind) -> Option<&str> {
        self.get_contents(which).get_text()
    }

    /// Convenience: set plain text on a clipboard.
    pub fn set_text(&mut self, which: ClipboardKind, text: String, owner: Option<PeerId>) {
        self.set_contents(which, ClipboardContents::with_text(text, owner));
    }

    /// Check whether a clipboard has data for a given flavor.
    pub fn has_flavor(&self, which: ClipboardKind, flavor: &DataFlavor) -> bool {
        self.get_contents(which).has_flavor(flavor)
    }

    /// List all flavors available on a clipboard.
    pub fn available_flavors(&self, which: ClipboardKind) -> Vec<DataFlavor> {
        self.get_contents(which).available_flavors()
    }
}

impl Default for ClipboardManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Global singleton
// ---------------------------------------------------------------------------

static CLIPBOARD: OnceLock<Mutex<ClipboardManager>> = OnceLock::new();

/// Returns the global clipboard manager.
pub fn get_clipboard() -> &'static Mutex<ClipboardManager> {
    CLIPBOARD.get_or_init(|| Mutex::new(ClipboardManager::new()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PeerId;

    #[test]
    fn empty_clipboard() {
        let mgr = ClipboardManager::new();
        assert!(mgr.get_text(ClipboardKind::System).is_none());
        assert!(mgr.get_text(ClipboardKind::Selection).is_none());
        assert!(mgr.available_flavors(ClipboardKind::System).is_empty());
    }

    #[test]
    fn set_and_get_text_system() {
        let mut mgr = ClipboardManager::new();
        mgr.set_text(ClipboardKind::System, "hello".to_string(), Some(PeerId(1)));

        assert_eq!(mgr.get_text(ClipboardKind::System), Some("hello"));
        // Selection should be unaffected.
        assert!(mgr.get_text(ClipboardKind::Selection).is_none());
    }

    #[test]
    fn set_and_get_text_selection() {
        let mut mgr = ClipboardManager::new();
        mgr.set_text(ClipboardKind::Selection, "world".to_string(), None);
        assert_eq!(mgr.get_text(ClipboardKind::Selection), Some("world"));
        assert!(mgr.get_text(ClipboardKind::System).is_none());
    }

    #[test]
    fn text_round_trip_with_both_flavors() {
        let mut mgr = ClipboardManager::new();
        mgr.set_text(ClipboardKind::System, "test".to_string(), None);

        assert!(mgr.has_flavor(ClipboardKind::System, &DataFlavor::StringFlavor));
        assert!(mgr.has_flavor(ClipboardKind::System, &DataFlavor::PlainTextFlavor));
        assert!(!mgr.has_flavor(ClipboardKind::System, &DataFlavor::ImageFlavor));
    }

    #[test]
    fn overwrite_clipboard() {
        let mut mgr = ClipboardManager::new();
        mgr.set_text(ClipboardKind::System, "first".to_string(), None);
        mgr.set_text(ClipboardKind::System, "second".to_string(), None);
        assert_eq!(mgr.get_text(ClipboardKind::System), Some("second"));
    }

    #[test]
    fn set_contents_with_image() {
        let mut mgr = ClipboardManager::new();
        let mut contents = ClipboardContents::new();
        contents.set(
            DataFlavor::ImageFlavor,
            ClipboardData::Image {
                pixels: vec![0xFF000000; 4],
                width: 2,
                height: 2,
            },
        );
        contents.set_owner(Some(PeerId(5)));

        mgr.set_contents(ClipboardKind::System, contents);

        assert!(mgr.has_flavor(ClipboardKind::System, &DataFlavor::ImageFlavor));
        assert!(!mgr.has_flavor(ClipboardKind::System, &DataFlavor::StringFlavor));

        let c = mgr.get_contents(ClipboardKind::System);
        assert_eq!(c.owner(), Some(PeerId(5)));

        if let Some(ClipboardData::Image {
            pixels,
            width,
            height,
        }) = c.get(&DataFlavor::ImageFlavor)
        {
            assert_eq!(*width, 2);
            assert_eq!(*height, 2);
            assert_eq!(pixels.len(), 4);
        } else {
            panic!("expected Image data");
        }
    }

    #[test]
    fn file_list_flavor() {
        let mut mgr = ClipboardManager::new();
        let mut contents = ClipboardContents::new();
        contents.set(
            DataFlavor::FileListFlavor,
            ClipboardData::FileList(vec!["/tmp/a.txt".to_string(), "/tmp/b.txt".to_string()]),
        );
        mgr.set_contents(ClipboardKind::System, contents);

        assert!(mgr.has_flavor(ClipboardKind::System, &DataFlavor::FileListFlavor));
        let c = mgr.get_contents(ClipboardKind::System);
        if let Some(ClipboardData::FileList(files)) = c.get(&DataFlavor::FileListFlavor) {
            assert_eq!(files.len(), 2);
            assert_eq!(files[0], "/tmp/a.txt");
        } else {
            panic!("expected FileList");
        }
    }

    #[test]
    fn custom_flavor() {
        let mut mgr = ClipboardManager::new();
        let flavor = DataFlavor::Custom("application/json".to_string());
        let mut contents = ClipboardContents::new();
        contents.set(
            flavor.clone(),
            ClipboardData::Raw(b"{\"key\":\"value\"}".to_vec()),
        );
        mgr.set_contents(ClipboardKind::System, contents);

        assert!(mgr.has_flavor(ClipboardKind::System, &flavor));
        let flavors = mgr.available_flavors(ClipboardKind::System);
        assert_eq!(flavors.len(), 1);
    }

    #[test]
    fn available_flavors_lists_all() {
        let mut contents = ClipboardContents::new();
        contents.set(DataFlavor::StringFlavor, ClipboardData::Text("hi".into()));
        contents.set(
            DataFlavor::ImageFlavor,
            ClipboardData::Image {
                pixels: vec![],
                width: 0,
                height: 0,
            },
        );

        let flavors = contents.available_flavors();
        assert_eq!(flavors.len(), 2);
        assert!(flavors.contains(&DataFlavor::StringFlavor));
        assert!(flavors.contains(&DataFlavor::ImageFlavor));
    }

    #[test]
    fn clipboard_contents_default_is_empty() {
        let c = ClipboardContents::default();
        assert!(c.get_text().is_none());
        assert!(c.available_flavors().is_empty());
        assert_eq!(c.owner(), None);
    }

    #[test]
    fn global_singleton_accessible() {
        let clip = get_clipboard();
        let mut mgr = clip.lock();
        mgr.set_text(ClipboardKind::System, "global-test".to_string(), None);
        assert_eq!(mgr.get_text(ClipboardKind::System), Some("global-test"));
    }
}
