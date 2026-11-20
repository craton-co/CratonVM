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

/// gc-common w23-a: repeated whole-bounds repaints queue one rectangle,
/// not one per call; a forgotten peer leaves no row or reference behind.
#[test]
fn w23a_dirty_regions_dedupe_and_forget_peers() {
    let mut s = SwingState::new();
    let (a, b) = (PeerId(0x0A23_01), PeerId(0x0A23_02));
    for _ in 0..1000 {
        s.mark_dirty(a, 0, 0, 640, 480);
    }
    assert_eq!(s.dirty_regions.get(&a).unwrap().len(), 1);
    s.mark_dirty(b, 0, 0, 10, 10);
    s.request_focus(a);
    s.active_window = Some(a);
    s.push_popup(a);
    s.push_popup(b);
    s.show_tooltip(a, "tip".into());
    s.forget_peers(&[a]);
    assert!(!s.dirty_regions.contains_key(&a));
    assert!(s.dirty_regions.contains_key(&b));
    assert_eq!(s.focus_owner, None);
    assert_eq!(s.active_window, None);
    assert_eq!(s.popups, vec![b]);
    assert!(!s.tooltip_visible && s.tooltip_peer.is_none());
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
