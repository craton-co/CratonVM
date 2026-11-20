// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! AWT event model — maps Java AWT events to Rust structures.
//!
//! Every constant in this module matches its Java counterpart exactly, so
//! native code can use them directly when constructing events that will be
//! delivered to Java listeners.

/// Peer identifier — wraps the u64 handle that the peer registry assigns to
/// each heavyweight component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub u64);

// ---------------------------------------------------------------------------
// AWT event ID constants (java.awt.event.*)
// ---------------------------------------------------------------------------

/// AWT event IDs (match java.awt.event.* constants).
pub mod event_id {
    // Component events
    pub const COMPONENT_MOVED: i32 = 100;
    pub const COMPONENT_RESIZED: i32 = 101;
    pub const COMPONENT_SHOWN: i32 = 102;
    pub const COMPONENT_HIDDEN: i32 = 103;

    // Window events
    pub const WINDOW_OPENED: i32 = 200;
    pub const WINDOW_CLOSING: i32 = 201;
    pub const WINDOW_CLOSED: i32 = 202;
    pub const WINDOW_ACTIVATED: i32 = 205;
    pub const WINDOW_DEACTIVATED: i32 = 206;
    pub const WINDOW_GAINED_FOCUS: i32 = 207;
    pub const WINDOW_LOST_FOCUS: i32 = 208;

    // Key events
    pub const KEY_TYPED: i32 = 400;
    pub const KEY_PRESSED: i32 = 401;
    pub const KEY_RELEASED: i32 = 402;

    // Mouse events
    pub const MOUSE_CLICKED: i32 = 500;
    pub const MOUSE_PRESSED: i32 = 501;
    pub const MOUSE_RELEASED: i32 = 502;
    pub const MOUSE_MOVED: i32 = 503;
    pub const MOUSE_ENTERED: i32 = 504;
    pub const MOUSE_EXITED: i32 = 505;
    pub const MOUSE_DRAGGED: i32 = 506;
    pub const MOUSE_WHEEL: i32 = 507;

    // Item events
    pub const ITEM_STATE_CHANGED: i32 = 701;

    // Paint events
    pub const PAINT: i32 = 800;
    pub const UPDATE: i32 = 801;

    // Action events
    pub const ACTION_PERFORMED: i32 = 1001;

    // Focus events
    pub const FOCUS_GAINED: i32 = 1004;
    pub const FOCUS_LOST: i32 = 1005;

    // Invocation events
    pub const INVOCATION_DEFAULT: i32 = 1200;
}

// ---------------------------------------------------------------------------
// Virtual key-code constants (java.awt.event.KeyEvent.VK_*)
// ---------------------------------------------------------------------------

/// VK constants (match `java.awt.event.KeyEvent.VK_*`).
pub mod vk {
    pub const VK_BACK_SPACE: i32 = 8;
    pub const VK_TAB: i32 = 9;
    pub const VK_ENTER: i32 = 10;
    pub const VK_SHIFT: i32 = 16;
    pub const VK_CONTROL: i32 = 17;
    pub const VK_ALT: i32 = 18;
    pub const VK_ESCAPE: i32 = 27;
    pub const VK_SPACE: i32 = 32;
    pub const VK_PAGE_UP: i32 = 33;
    pub const VK_PAGE_DOWN: i32 = 34;
    pub const VK_END: i32 = 35;
    pub const VK_HOME: i32 = 36;
    pub const VK_LEFT: i32 = 37;
    pub const VK_UP: i32 = 38;
    pub const VK_RIGHT: i32 = 39;
    pub const VK_DOWN: i32 = 40;
    pub const VK_0: i32 = 48;
    pub const VK_9: i32 = 57;
    pub const VK_A: i32 = 65;
    pub const VK_Z: i32 = 90;
    pub const VK_F1: i32 = 112;
    pub const VK_DELETE: i32 = 127;
}

// ---------------------------------------------------------------------------
// Input modifier masks (java.awt.event.InputEvent)
// ---------------------------------------------------------------------------

/// Input modifier mask bits (match `java.awt.event.InputEvent`).
pub mod modifiers {
    pub const SHIFT_DOWN_MASK: i32 = 1 << 6; // 64
    pub const CTRL_DOWN_MASK: i32 = 1 << 7; // 128
    pub const META_DOWN_MASK: i32 = 1 << 8; // 256
    pub const ALT_DOWN_MASK: i32 = 1 << 9; // 512
    pub const BUTTON1_DOWN_MASK: i32 = 1 << 10; // 1024
    pub const BUTTON2_DOWN_MASK: i32 = 1 << 11; // 2048
    pub const BUTTON3_DOWN_MASK: i32 = 1 << 12; // 4096
}

// ---------------------------------------------------------------------------
// Mouse button constants
// ---------------------------------------------------------------------------

pub const NOBUTTON: i32 = 0;
pub const BUTTON1: i32 = 1;
pub const BUTTON2: i32 = 2;
pub const BUTTON3: i32 = 3;

// ---------------------------------------------------------------------------
// Event structures
// ---------------------------------------------------------------------------

/// An AWT event ready to be dispatched to a Java component.
#[derive(Debug, Clone)]
pub struct AwtEvent {
    /// One of the [`event_id`] constants.
    pub id: i32,
    /// Which component this targets.
    pub source_peer_id: PeerId,
    /// Timestamp (`System.currentTimeMillis()` equivalent).
    pub timestamp: u64,
    /// Type-specific payload.
    pub data: AwtEventData,
}

/// Type-specific event payload.
#[derive(Debug, Clone)]
pub enum AwtEventData {
    Mouse {
        x: i32,
        y: i32,
        button: i32,
        click_count: i32,
        modifiers: i32,
        /// Non-zero for `MOUSE_WHEEL` events.
        scroll_amount: i32,
    },
    Key {
        key_code: i32,
        key_char: char,
        modifiers: i32,
    },
    Component {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    Focus {
        temporary: bool,
    },
    Window,
    Action {
        command: String,
    },
    Paint {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    Invocation {
        /// Callback ID -- the EDT will invoke the Java `Runnable` registered
        /// under this ID.
        callback_id: u64,
    },
}

// ---------------------------------------------------------------------------
// Convenience constructors
// ---------------------------------------------------------------------------

impl AwtEvent {
    /// Create a mouse event.
    pub fn mouse(
        id: i32,
        peer: PeerId,
        timestamp: u64,
        x: i32,
        y: i32,
        button: i32,
        click_count: i32,
        mods: i32,
    ) -> Self {
        Self {
            id,
            source_peer_id: peer,
            timestamp,
            data: AwtEventData::Mouse {
                x,
                y,
                button,
                click_count,
                modifiers: mods,
                scroll_amount: 0,
            },
        }
    }

    /// Create a mouse-wheel event.
    pub fn mouse_wheel(
        peer: PeerId,
        timestamp: u64,
        x: i32,
        y: i32,
        mods: i32,
        scroll_amount: i32,
    ) -> Self {
        Self {
            id: event_id::MOUSE_WHEEL,
            source_peer_id: peer,
            timestamp,
            data: AwtEventData::Mouse {
                x,
                y,
                button: NOBUTTON,
                click_count: 0,
                modifiers: mods,
                scroll_amount,
            },
        }
    }

    /// Create a key event.
    pub fn key(
        id: i32,
        peer: PeerId,
        timestamp: u64,
        key_code: i32,
        key_char: char,
        mods: i32,
    ) -> Self {
        Self {
            id,
            source_peer_id: peer,
            timestamp,
            data: AwtEventData::Key {
                key_code,
                key_char,
                modifiers: mods,
            },
        }
    }

    /// Create a component event (moved/resized/shown/hidden).
    pub fn component(
        id: i32,
        peer: PeerId,
        timestamp: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Self {
        Self {
            id,
            source_peer_id: peer,
            timestamp,
            data: AwtEventData::Component {
                x,
                y,
                width,
                height,
            },
        }
    }

    /// Create a focus event.
    pub fn focus(id: i32, peer: PeerId, timestamp: u64, temporary: bool) -> Self {
        Self {
            id,
            source_peer_id: peer,
            timestamp,
            data: AwtEventData::Focus { temporary },
        }
    }

    /// Create a window event.
    pub fn window(id: i32, peer: PeerId, timestamp: u64) -> Self {
        Self {
            id,
            source_peer_id: peer,
            timestamp,
            data: AwtEventData::Window,
        }
    }

    /// Create an action event.
    pub fn action(peer: PeerId, timestamp: u64, command: String) -> Self {
        Self {
            id: event_id::ACTION_PERFORMED,
            source_peer_id: peer,
            timestamp,
            data: AwtEventData::Action { command },
        }
    }

    /// Create a paint event.
    pub fn paint(
        id: i32,
        peer: PeerId,
        timestamp: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Self {
        Self {
            id,
            source_peer_id: peer,
            timestamp,
            data: AwtEventData::Paint {
                x,
                y,
                width,
                height,
            },
        }
    }

    /// Create an invocation event (for `invokeLater` / `invokeAndWait`).
    pub fn invocation(peer: PeerId, timestamp: u64, callback_id: u64) -> Self {
        Self {
            id: event_id::INVOCATION_DEFAULT,
            source_peer_id: peer,
            timestamp,
            data: AwtEventData::Invocation { callback_id },
        }
    }

    /// Returns `true` if this is a paint or update event.
    pub fn is_paint(&self) -> bool {
        self.id == event_id::PAINT || self.id == event_id::UPDATE
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
