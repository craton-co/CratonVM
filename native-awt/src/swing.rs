// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Swing component state management and Metal Look-and-Feel defaults.
//!
//! Provides:
//! - [`MetalTheme`] — the Ocean and Steel color themes
//! - [`UIDefaults`] — the property table backing `javax.swing.UIDefaults`
//! - [`SwingState`] — focus management, repaint coalescing, popup/tooltip state

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::peer::PeerId;

// ══════════════════════════════════════════════════════════════════════════
// MetalTheme
// ══════════════════════════════════════════════════════════════════════════

/// Metal Look-and-Feel color scheme (matches `javax.swing.plaf.metal.MetalLookAndFeel`).
pub struct MetalTheme {
    pub primary1: u32,
    pub primary2: u32,
    pub primary3: u32,
    pub secondary1: u32,
    pub secondary2: u32,
    pub secondary3: u32,
    pub black: u32,
    pub white: u32,
    pub control: u32,
    pub control_shadow: u32,
    pub control_dark_shadow: u32,
    pub control_highlight: u32,
    pub text: u32,
    pub text_highlight: u32,
    pub accelerator_foreground: u32,
    pub menu_background: u32,
    pub menu_foreground: u32,
    pub menu_selected_background: u32,
    pub menu_selected_foreground: u32,
    pub window_background: u32,
    pub window_title_background: u32,
    pub window_title_foreground: u32,
}

impl MetalTheme {
    /// The default "Ocean" theme colors (Java SE 5+).
    pub fn ocean() -> Self {
        Self {
            primary1: 0xFF336699,
            primary2: 0xFF6699CC,
            primary3: 0xFFB3D4FF,
            secondary1: 0xFF7A8A99,
            secondary2: 0xFFB8CFE5,
            secondary3: 0xFFEEEEEE,
            black: 0xFF000000,
            white: 0xFFFFFFFF,
            control: 0xFFEEEEEE,
            control_shadow: 0xFF7A8A99,
            control_dark_shadow: 0xFF333333,
            control_highlight: 0xFFFFFFFF,
            text: 0xFF000000,
            text_highlight: 0xFFB3D4FF,
            accelerator_foreground: 0xFF336699,
            menu_background: 0xFFEEEEEE,
            menu_foreground: 0xFF000000,
            menu_selected_background: 0xFF6699CC,
            menu_selected_foreground: 0xFF000000,
            window_background: 0xFFFFFFFF,
            window_title_background: 0xFFB3D4FF,
            window_title_foreground: 0xFF000000,
        }
    }

    /// The classic "Steel" theme (pre-Java SE 5).
    pub fn steel() -> Self {
        Self {
            primary1: 0xFF666699,
            primary2: 0xFF9999CC,
            primary3: 0xFFCCCCFF,
            secondary1: 0xFF666666,
            secondary2: 0xFF999999,
            secondary3: 0xFFCCCCCC,
            black: 0xFF000000,
            white: 0xFFFFFFFF,
            control: 0xFFCCCCCC,
            control_shadow: 0xFF666666,
            control_dark_shadow: 0xFF333333,
            control_highlight: 0xFFFFFFFF,
            text: 0xFF000000,
            text_highlight: 0xFFCCCCFF,
            accelerator_foreground: 0xFF666699,
            menu_background: 0xFFCCCCCC,
            menu_foreground: 0xFF000000,
            menu_selected_background: 0xFF9999CC,
            menu_selected_foreground: 0xFF000000,
            window_background: 0xFFFFFFFF,
            window_title_background: 0xFFCCCCFF,
            window_title_foreground: 0xFF000000,
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════
// UIDefaults
// ══════════════════════════════════════════════════════════════════════════

/// UI defaults table (matches `javax.swing.UIDefaults`).
///
/// Maps property keys to default values for the Metal Look-and-Feel.
#[derive(Clone)]
pub struct UIDefaults {
    colors: HashMap<String, u32>,
    fonts: HashMap<String, (String, i32, i32)>, // (family, style, size)
    insets: HashMap<String, (i32, i32, i32, i32)>, // top, left, bottom, right
    dimensions: HashMap<String, (i32, i32)>,
    booleans: HashMap<String, bool>,
    integers: HashMap<String, i32>,
}

impl UIDefaults {
    /// Build a new UIDefaults table populated with all Metal/Ocean defaults
    /// for standard Swing components.
    pub fn new_metal() -> Self {
        let theme = MetalTheme::ocean();
        let mut defaults = Self {
            colors: HashMap::new(),
            fonts: HashMap::new(),
            insets: HashMap::new(),
            dimensions: HashMap::new(),
            booleans: HashMap::new(),
            integers: HashMap::new(),
        };

        // Font constants
        let dialog_plain_12 = ("Dialog".to_string(), 0, 12);
        let dialog_bold_12 = ("Dialog".to_string(), 1, 12);
        let dialog_plain_10 = ("Dialog".to_string(), 0, 10);
        let monospaced_plain_12 = ("Monospaced".to_string(), 0, 12);

        // ── Button ──────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("Button.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("Button.background".into(), theme.control);
        defaults
            .colors
            .insert("Button.foreground".into(), theme.text);
        defaults
            .colors
            .insert("Button.select".into(), theme.control_shadow);
        defaults
            .colors
            .insert("Button.disabledText".into(), theme.secondary1);
        defaults
            .colors
            .insert("Button.focus".into(), theme.primary2);
        defaults
            .insets
            .insert("Button.margin".into(), (2, 14, 2, 14));

        // ── Label ───────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("Label.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("Label.foreground".into(), theme.text);
        defaults
            .colors
            .insert("Label.background".into(), theme.control);
        defaults
            .colors
            .insert("Label.disabledForeground".into(), theme.secondary1);

        // ── TextField ───────────────────────────────────────────────────
        defaults
            .fonts
            .insert("TextField.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("TextField.background".into(), theme.white);
        defaults
            .colors
            .insert("TextField.foreground".into(), theme.text);
        defaults
            .colors
            .insert("TextField.caretForeground".into(), theme.text);
        defaults
            .colors
            .insert("TextField.inactiveBackground".into(), theme.secondary3);
        defaults
            .colors
            .insert("TextField.inactiveForeground".into(), theme.secondary1);
        defaults
            .colors
            .insert("TextField.selectionBackground".into(), theme.primary3);
        defaults
            .colors
            .insert("TextField.selectionForeground".into(), theme.text);
        defaults
            .insets
            .insert("TextField.margin".into(), (0, 0, 0, 0));

        // ── TextArea ────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("TextArea.font".into(), monospaced_plain_12.clone());
        defaults
            .colors
            .insert("TextArea.background".into(), theme.white);
        defaults
            .colors
            .insert("TextArea.foreground".into(), theme.text);
        defaults
            .colors
            .insert("TextArea.caretForeground".into(), theme.text);
        defaults
            .colors
            .insert("TextArea.selectionBackground".into(), theme.primary3);
        defaults
            .colors
            .insert("TextArea.selectionForeground".into(), theme.text);

        // ── Table ───────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("Table.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("Table.background".into(), theme.white);
        defaults
            .colors
            .insert("Table.foreground".into(), theme.text);
        defaults
            .colors
            .insert("Table.gridColor".into(), theme.secondary2);
        defaults
            .colors
            .insert("Table.selectionBackground".into(), theme.primary3);
        defaults
            .colors
            .insert("Table.selectionForeground".into(), theme.text);
        defaults.integers.insert("Table.rowHeight".into(), 16);

        // ── Tree ────────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("Tree.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("Tree.background".into(), theme.white);
        defaults.colors.insert("Tree.foreground".into(), theme.text);
        defaults
            .colors
            .insert("Tree.selectionBackground".into(), theme.primary3);
        defaults
            .colors
            .insert("Tree.selectionForeground".into(), theme.text);
        defaults.colors.insert("Tree.hash".into(), theme.secondary2);
        defaults.colors.insert("Tree.line".into(), theme.secondary2);
        defaults.integers.insert("Tree.rowHeight".into(), 16);

        // ── Panel ───────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("Panel.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("Panel.background".into(), theme.control);
        defaults
            .colors
            .insert("Panel.foreground".into(), theme.text);

        // ── OptionPane ──────────────────────────────────────────────────
        defaults
            .fonts
            .insert("OptionPane.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("OptionPane.background".into(), theme.control);
        defaults
            .colors
            .insert("OptionPane.messageForeground".into(), theme.text);
        defaults
            .fonts
            .insert("OptionPane.messageFont".into(), dialog_plain_12.clone());
        defaults
            .fonts
            .insert("OptionPane.buttonFont".into(), dialog_plain_12.clone());

        // ── ScrollPane / ScrollBar ──────────────────────────────────────
        defaults
            .colors
            .insert("ScrollPane.background".into(), theme.control);
        defaults
            .colors
            .insert("ScrollBar.background".into(), theme.secondary3);
        defaults
            .colors
            .insert("ScrollBar.thumb".into(), theme.primary2);
        defaults
            .colors
            .insert("ScrollBar.thumbShadow".into(), theme.primary1);
        defaults
            .colors
            .insert("ScrollBar.thumbHighlight".into(), theme.primary3);
        defaults
            .colors
            .insert("ScrollBar.track".into(), theme.secondary3);
        defaults.integers.insert("ScrollBar.width".into(), 17);

        // ── Menu / MenuBar / MenuItem ───────────────────────────────────
        defaults
            .fonts
            .insert("Menu.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("Menu.background".into(), theme.menu_background);
        defaults
            .colors
            .insert("Menu.foreground".into(), theme.menu_foreground);
        defaults.colors.insert(
            "Menu.selectionBackground".into(),
            theme.menu_selected_background,
        );
        defaults.colors.insert(
            "Menu.selectionForeground".into(),
            theme.menu_selected_foreground,
        );
        defaults.colors.insert(
            "Menu.acceleratorForeground".into(),
            theme.accelerator_foreground,
        );
        defaults
            .colors
            .insert("MenuBar.background".into(), theme.menu_background);
        defaults
            .colors
            .insert("MenuBar.foreground".into(), theme.menu_foreground);
        defaults
            .fonts
            .insert("MenuBar.font".into(), dialog_plain_12.clone());
        defaults
            .fonts
            .insert("MenuItem.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("MenuItem.background".into(), theme.menu_background);
        defaults
            .colors
            .insert("MenuItem.foreground".into(), theme.menu_foreground);
        defaults.colors.insert(
            "MenuItem.selectionBackground".into(),
            theme.menu_selected_background,
        );
        defaults.colors.insert(
            "MenuItem.selectionForeground".into(),
            theme.menu_selected_foreground,
        );
        defaults.colors.insert(
            "MenuItem.acceleratorForeground".into(),
            theme.accelerator_foreground,
        );

        // ── FileChooser ─────────────────────────────────────────────────
        defaults
            .colors
            .insert("FileChooser.detailsViewIcon".into(), theme.primary2);
        defaults
            .colors
            .insert("FileChooser.homeFolderIcon".into(), theme.primary2);
        defaults
            .colors
            .insert("FileChooser.listViewIcon".into(), theme.primary2);
        defaults
            .colors
            .insert("FileChooser.newFolderIcon".into(), theme.primary2);
        defaults
            .colors
            .insert("FileChooser.upFolderIcon".into(), theme.primary2);

        // ── TabbedPane ──────────────────────────────────────────────────
        defaults
            .fonts
            .insert("TabbedPane.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("TabbedPane.background".into(), theme.control);
        defaults
            .colors
            .insert("TabbedPane.foreground".into(), theme.text);
        defaults
            .colors
            .insert("TabbedPane.selected".into(), theme.primary3);
        defaults
            .colors
            .insert("TabbedPane.highlight".into(), theme.control_highlight);
        defaults
            .colors
            .insert("TabbedPane.shadow".into(), theme.control_shadow);
        defaults
            .insets
            .insert("TabbedPane.tabInsets".into(), (0, 4, 1, 4));
        defaults
            .insets
            .insert("TabbedPane.contentBorderInsets".into(), (2, 2, 3, 3));

        // ── CheckBox ────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("CheckBox.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("CheckBox.background".into(), theme.control);
        defaults
            .colors
            .insert("CheckBox.foreground".into(), theme.text);
        defaults
            .colors
            .insert("CheckBox.focus".into(), theme.primary2);

        // ── ComboBox ────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("ComboBox.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("ComboBox.background".into(), theme.white);
        defaults
            .colors
            .insert("ComboBox.foreground".into(), theme.text);
        defaults
            .colors
            .insert("ComboBox.selectionBackground".into(), theme.primary3);
        defaults
            .colors
            .insert("ComboBox.selectionForeground".into(), theme.text);

        // ── Spinner ─────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("Spinner.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("Spinner.background".into(), theme.secondary3);
        defaults
            .colors
            .insert("Spinner.foreground".into(), theme.text);

        // ── Slider ──────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("Slider.font".into(), dialog_plain_10.clone());
        defaults
            .colors
            .insert("Slider.background".into(), theme.control);
        defaults
            .colors
            .insert("Slider.foreground".into(), theme.primary2);
        defaults
            .colors
            .insert("Slider.focus".into(), theme.primary2);

        // ── ProgressBar ─────────────────────────────────────────────────
        defaults
            .fonts
            .insert("ProgressBar.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("ProgressBar.background".into(), theme.secondary3);
        defaults
            .colors
            .insert("ProgressBar.foreground".into(), theme.primary2);
        defaults
            .colors
            .insert("ProgressBar.selectionBackground".into(), theme.primary1);
        defaults
            .colors
            .insert("ProgressBar.selectionForeground".into(), theme.secondary3);

        // ── TitledBorder ────────────────────────────────────────────────
        defaults
            .fonts
            .insert("TitledBorder.font".into(), dialog_bold_12.clone());
        defaults
            .colors
            .insert("TitledBorder.titleColor".into(), theme.primary1);

        // ── ToolTip ─────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("ToolTip.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("ToolTip.background".into(), 0xFFFFFFE1); // light yellow
        defaults
            .colors
            .insert("ToolTip.foreground".into(), theme.text);

        // ── List ────────────────────────────────────────────────────────
        defaults
            .fonts
            .insert("List.font".into(), dialog_plain_12.clone());
        defaults
            .colors
            .insert("List.background".into(), theme.white);
        defaults.colors.insert("List.foreground".into(), theme.text);
        defaults
            .colors
            .insert("List.selectionBackground".into(), theme.primary3);
        defaults
            .colors
            .insert("List.selectionForeground".into(), theme.text);

        // ── EditorPane / TextPane ───────────────────────────────────────
        defaults
            .fonts
            .insert("EditorPane.font".into(), dialog_plain_12.clone());
        defaults
            .fonts
            .insert("TextPane.font".into(), dialog_plain_12.clone());

        // ── ToolBar ─────────────────────────────────────────────────────
        defaults
            .colors
            .insert("ToolBar.background".into(), theme.control);
        defaults
            .colors
            .insert("ToolBar.foreground".into(), theme.text);

        // ── SplitPane ───────────────────────────────────────────────────
        defaults
            .colors
            .insert("SplitPane.background".into(), theme.control);
        defaults.integers.insert("SplitPane.dividerSize".into(), 7);

        // ── InternalFrame ───────────────────────────────────────────────
        defaults.colors.insert(
            "InternalFrame.activeTitleBackground".into(),
            theme.window_title_background,
        );
        defaults.colors.insert(
            "InternalFrame.activeTitleForeground".into(),
            theme.window_title_foreground,
        );
        defaults.colors.insert(
            "InternalFrame.inactiveTitleBackground".into(),
            theme.secondary3,
        );
        defaults.colors.insert(
            "InternalFrame.inactiveTitleForeground".into(),
            theme.secondary1,
        );

        // ── Global defaults ─────────────────────────────────────────────
        defaults.colors.insert("control".into(), theme.control);
        defaults
            .colors
            .insert("controlShadow".into(), theme.control_shadow);
        defaults
            .colors
            .insert("controlDkShadow".into(), theme.control_dark_shadow);
        defaults
            .colors
            .insert("controlHighlight".into(), theme.control_highlight);
        defaults.colors.insert("text".into(), theme.text);
        defaults
            .colors
            .insert("textHighlight".into(), theme.text_highlight);
        defaults
            .colors
            .insert("window".into(), theme.window_background);
        defaults
            .colors
            .insert("activeCaption".into(), theme.window_title_background);
        defaults
            .colors
            .insert("activeCaptionText".into(), theme.window_title_foreground);

        // ── Booleans ────────────────────────────────────────────────────
        defaults
            .booleans
            .insert("Button.defaultButtonFollowsFocus".into(), true);
        defaults
            .booleans
            .insert("Table.scrollPaneBorder.allowAutoBorder".into(), true);

        // ── Dimensions ──────────────────────────────────────────────────
        defaults
            .dimensions
            .insert("Slider.minimumHorizontalSize".into(), (36, 21));
        defaults
            .dimensions
            .insert("Slider.minimumVerticalSize".into(), (21, 36));
        defaults
            .dimensions
            .insert("ProgressBar.horizontalSize".into(), (146, 12));
        defaults
            .dimensions
            .insert("ProgressBar.verticalSize".into(), (12, 146));

        defaults
    }

    // ── Getters ─────────────────────────────────────────────────────────

    pub fn get_color(&self, key: &str) -> Option<u32> {
        self.colors.get(key).copied()
    }

    pub fn get_font(&self, key: &str) -> Option<(String, i32, i32)> {
        self.fonts.get(key).cloned()
    }

    pub fn get_insets(&self, key: &str) -> Option<(i32, i32, i32, i32)> {
        self.insets.get(key).copied()
    }

    pub fn get_dimension(&self, key: &str) -> Option<(i32, i32)> {
        self.dimensions.get(key).copied()
    }

    pub fn get_boolean(&self, key: &str) -> Option<bool> {
        self.booleans.get(key).copied()
    }

    pub fn get_integer(&self, key: &str) -> Option<i32> {
        self.integers.get(key).copied()
    }

    // ── Setters ─────────────────────────────────────────────────────────

    pub fn put_color(&mut self, key: &str, value: u32) {
        self.colors.insert(key.to_string(), value);
    }

    pub fn put_font(&mut self, key: &str, family: &str, style: i32, size: i32) {
        self.fonts
            .insert(key.to_string(), (family.to_string(), style, size));
    }

    pub fn put_insets(&mut self, key: &str, top: i32, left: i32, bottom: i32, right: i32) {
        self.insets
            .insert(key.to_string(), (top, left, bottom, right));
    }

    pub fn put_dimension(&mut self, key: &str, width: i32, height: i32) {
        self.dimensions.insert(key.to_string(), (width, height));
    }

    pub fn put_boolean(&mut self, key: &str, value: bool) {
        self.booleans.insert(key.to_string(), value);
    }

    pub fn put_integer(&mut self, key: &str, value: i32) {
        self.integers.insert(key.to_string(), value);
    }
}

// ══════════════════════════════════════════════════════════════════════════
// LookAndFeel
// ══════════════════════════════════════════════════════════════════════════

/// Active look-and-feel.
#[derive(Debug, Clone, PartialEq)]
pub enum LookAndFeel {
    Metal,
    // Future: Nimbus, GTK, System
}

// ══════════════════════════════════════════════════════════════════════════
// SwingState
// ══════════════════════════════════════════════════════════════════════════

/// Swing component state tracking for repaint coalescing and focus management.
pub struct SwingState {
    pub current_laf: LookAndFeel,
    pub defaults: UIDefaults,
    pub focus_owner: Option<PeerId>,
    pub focus_cycle_root: Option<PeerId>,
    pub active_window: Option<PeerId>,
    /// Dirty regions needing repaint (component -> list of bounds).
    pub dirty_regions: HashMap<PeerId, Vec<(i32, i32, u32, u32)>>,
    /// Popup layer tracking (stack of visible popups).
    pub popups: Vec<PeerId>,
    /// Tooltip state.
    pub tooltip_peer: Option<PeerId>,
    pub tooltip_text: Option<String>,
    pub tooltip_visible: bool,
}

impl SwingState {
    /// Create a new SwingState with Metal L&F defaults.
    pub fn new() -> Self {
        Self {
            current_laf: LookAndFeel::Metal,
            defaults: UIDefaults::new_metal(),
            focus_owner: None,
            focus_cycle_root: None,
            active_window: None,
            dirty_regions: HashMap::new(),
            popups: Vec::new(),
            tooltip_peer: None,
            tooltip_text: None,
            tooltip_visible: false,
        }
    }

    /// Request focus for the given peer.
    pub fn request_focus(&mut self, peer: PeerId) {
        self.focus_owner = Some(peer);
    }

    /// Queue a dirty region for the given peer.
    pub fn mark_dirty(&mut self, peer: PeerId, x: i32, y: i32, w: u32, h: u32) {
        self.dirty_regions
            .entry(peer)
            .or_default()
            .push((x, y, w, h));
    }

    /// Take all dirty regions, leaving the map empty.
    pub fn drain_dirty(&mut self) -> HashMap<PeerId, Vec<(i32, i32, u32, u32)>> {
        std::mem::take(&mut self.dirty_regions)
    }

    /// Show a tooltip on the given peer.
    pub fn show_tooltip(&mut self, peer: PeerId, text: String) {
        self.tooltip_peer = Some(peer);
        self.tooltip_text = Some(text);
        self.tooltip_visible = true;
    }

    /// Hide the current tooltip.
    pub fn hide_tooltip(&mut self) {
        self.tooltip_visible = false;
        self.tooltip_peer = None;
        self.tooltip_text = None;
    }

    /// Push a popup onto the popup stack.
    pub fn push_popup(&mut self, peer: PeerId) {
        self.popups.push(peer);
    }

    /// Pop the top popup from the stack.
    pub fn pop_popup(&mut self) -> Option<PeerId> {
        self.popups.pop()
    }
}

impl Default for SwingState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Global singleton ────────────────────────────────────────────────────

/// Access the global Swing state (locked).
pub fn swing_state() -> &'static Mutex<SwingState> {
    static INSTANCE: OnceLock<Mutex<SwingState>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(SwingState::new()))
}

// ══════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── MetalTheme ──────────────────────────────────────────────────────

    #[test]
    fn ocean_theme_colors() {
        let t = MetalTheme::ocean();
        assert_eq!(t.primary1, 0xFF336699);
        assert_eq!(t.secondary3, 0xFFEEEEEE);
        assert_eq!(t.white, 0xFFFFFFFF);
        assert_eq!(t.black, 0xFF000000);
    }

    #[test]
    fn steel_theme_differs_from_ocean() {
        let ocean = MetalTheme::ocean();
        let steel = MetalTheme::steel();
        assert_ne!(ocean.primary1, steel.primary1);
        assert_ne!(ocean.secondary3, steel.secondary3);
    }

    // ── UIDefaults ──────────────────────────────────────────────────────

    #[test]
    fn metal_defaults_has_button_font() {
        let d = UIDefaults::new_metal();
        let font = d.get_font("Button.font").unwrap();
        assert_eq!(font.0, "Dialog");
        assert_eq!(font.2, 12);
    }

    #[test]
    fn metal_defaults_has_button_background() {
        let d = UIDefaults::new_metal();
        let bg = d.get_color("Button.background").unwrap();
        assert_eq!(bg, 0xFFEEEEEE); // secondary3
    }

    #[test]
    fn metal_defaults_has_table_colors() {
        let d = UIDefaults::new_metal();
        assert!(d.get_color("Table.background").is_some());
        assert!(d.get_color("Table.selectionBackground").is_some());
    }

    #[test]
    fn metal_defaults_has_tree_row_height() {
        let d = UIDefaults::new_metal();
        assert_eq!(d.get_integer("Tree.rowHeight"), Some(16));
    }

    #[test]
    fn metal_defaults_has_tooltip_colors() {
        let d = UIDefaults::new_metal();
        let bg = d.get_color("ToolTip.background").unwrap();
        assert_eq!(bg, 0xFFFFFFE1); // light yellow
    }

    #[test]
    fn metal_defaults_has_tabbed_pane_insets() {
        let d = UIDefaults::new_metal();
        let insets = d.get_insets("TabbedPane.tabInsets").unwrap();
        assert_eq!(insets, (0, 4, 1, 4));
    }

    #[test]
    fn metal_defaults_has_booleans() {
        let d = UIDefaults::new_metal();
        assert_eq!(
            d.get_boolean("Button.defaultButtonFollowsFocus"),
            Some(true)
        );
    }

    #[test]
    fn metal_defaults_missing_key_returns_none() {
        let d = UIDefaults::new_metal();
        assert!(d.get_color("NonExistent.key").is_none());
        assert!(d.get_font("NonExistent.key").is_none());
        assert!(d.get_insets("NonExistent.key").is_none());
        assert!(d.get_dimension("NonExistent.key").is_none());
        assert!(d.get_boolean("NonExistent.key").is_none());
        assert!(d.get_integer("NonExistent.key").is_none());
    }

    #[test]
    fn put_overrides_defaults() {
        let mut d = UIDefaults::new_metal();
        d.put_color("Button.background", 0xFFFF0000);
        assert_eq!(d.get_color("Button.background"), Some(0xFFFF0000));
    }

    #[test]
    fn put_font() {
        let mut d = UIDefaults::new_metal();
        d.put_font("Custom.font", "Arial", 1, 14);
        let f = d.get_font("Custom.font").unwrap();
        assert_eq!(f.0, "Arial");
        assert_eq!(f.1, 1);
        assert_eq!(f.2, 14);
    }

    #[test]
    fn put_insets() {
        let mut d = UIDefaults::new_metal();
        d.put_insets("Custom.insets", 1, 2, 3, 4);
        assert_eq!(d.get_insets("Custom.insets"), Some((1, 2, 3, 4)));
    }

    #[test]
    fn put_dimension() {
        let mut d = UIDefaults::new_metal();
        d.put_dimension("Custom.size", 100, 200);
        assert_eq!(d.get_dimension("Custom.size"), Some((100, 200)));
    }

    #[test]
    fn put_boolean_and_integer() {
        let mut d = UIDefaults::new_metal();
        d.put_boolean("Custom.flag", false);
        d.put_integer("Custom.num", 42);
        assert_eq!(d.get_boolean("Custom.flag"), Some(false));
        assert_eq!(d.get_integer("Custom.num"), Some(42));
    }

    // ── SwingState ──────────────────────────────────────────────────────

    #[test]
    fn new_state_is_metal() {
        let s = SwingState::new();
        assert_eq!(s.current_laf, LookAndFeel::Metal);
        assert!(s.focus_owner.is_none());
        assert!(s.popups.is_empty());
    }

    #[test]
    fn request_focus() {
        let mut s = SwingState::new();
        let peer = PeerId(1);
        s.request_focus(peer);
        assert_eq!(s.focus_owner, Some(peer));
    }

    #[test]
    fn dirty_regions() {
        let mut s = SwingState::new();
        let peer = PeerId(1);
        s.mark_dirty(peer, 0, 0, 100, 50);
        s.mark_dirty(peer, 10, 10, 20, 20);
        assert_eq!(s.dirty_regions.get(&peer).unwrap().len(), 2);

        let drained = s.drain_dirty();
        assert_eq!(drained.get(&peer).unwrap().len(), 2);
        assert!(s.dirty_regions.is_empty());
    }

    #[test]
    fn tooltip_show_hide() {
        let mut s = SwingState::new();
        s.show_tooltip(PeerId(5), "Hello".into());
        assert!(s.tooltip_visible);
        assert_eq!(s.tooltip_text.as_deref(), Some("Hello"));
        assert_eq!(s.tooltip_peer, Some(PeerId(5)));

        s.hide_tooltip();
        assert!(!s.tooltip_visible);
        assert!(s.tooltip_text.is_none());
    }

    #[test]
    fn popup_stack() {
        let mut s = SwingState::new();
        s.push_popup(PeerId(1));
        s.push_popup(PeerId(2));
        assert_eq!(s.pop_popup(), Some(PeerId(2)));
        assert_eq!(s.pop_popup(), Some(PeerId(1)));
        assert_eq!(s.pop_popup(), None);
    }

    #[test]
    fn singleton_accessible() {
        let ss = swing_state();
        let _lock = ss.lock();
    }
}
