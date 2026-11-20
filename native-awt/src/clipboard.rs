// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
mod tests;
