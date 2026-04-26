//! AWT Component Peer system — manages native representations of Java
//! AWT/Swing components.
//!
//! Every Java AWT component gets a native "peer" that tracks its state
//! (position, size, visibility, etc.). Peers are organized in a parent-child
//! hierarchy mirroring the Java `Component` containment tree.
//!
//! The global [`PeerRegistry`] is the single source of truth for all live
//! peers. Java object identity is mapped to peer IDs via
//! `register_java_mapping` / `peer_for_java`.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

// Re-export PeerId from the event module to avoid duplicate types.
pub use crate::event::PeerId;

// ── ComponentType ───────────────────────────────────────────────────────

/// Enumerates all AWT/Swing heavyweight component kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum ComponentType {
    Frame,
    Dialog,
    Panel,
    Canvas,
    Button,
    Label,
    TextField,
    TextArea,
    Checkbox,
    Choice,
    List,
    Scrollbar,
    ScrollPane,
    MenuBar,
    Menu,
    MenuItem,
    PopupMenu,
    FileDialog,
}

// ── ComponentPeer ───────────────────────────────────────────────────────

/// A native peer for a Java AWT/Swing component.
#[derive(Debug)]
pub struct ComponentPeer {
    pub id: PeerId,
    pub component_type: ComponentType,
    pub parent_id: Option<PeerId>,
    pub children: Vec<PeerId>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
    pub enabled: bool,
    pub focusable: bool,
    pub has_focus: bool,
    pub background: u32,  // ARGB
    pub foreground: u32,  // ARGB
    pub font_family: String,
    pub font_style: i32,
    pub font_size: i32,
    pub title: String,     // for Frame/Dialog
    pub text: String,      // for Button/Label/TextField
    pub window_id: Option<u64>,  // platform WindowId, only for Frame/Dialog
    pub image_id: Option<u64>,   // backing BufferedImage for double-buffering
    pub resizable: bool,
    pub opaque: bool,
    pub cursor_type: i32,  // java.awt.Cursor.DEFAULT_CURSOR etc.
    pub minimum_size: (u32, u32),
    pub preferred_size: (u32, u32),
    pub maximum_size: (u32, u32),
}

impl ComponentPeer {
    /// Create a new peer with sensible defaults.
    pub fn new(id: PeerId, component_type: ComponentType) -> Self {
        let is_window = matches!(component_type, ComponentType::Frame | ComponentType::Dialog);
        Self {
            id,
            component_type,
            parent_id: None,
            children: Vec::new(),
            x: 0,
            y: 0,
            width: if is_window { 640 } else { 0 },
            height: if is_window { 480 } else { 0 },
            visible: false,
            enabled: true,
            focusable: true,
            has_focus: false,
            background: 0xFFEEEEEE, // Metal L&F default
            foreground: 0xFF000000, // black
            font_family: "Dialog".to_string(),
            font_style: 0, // PLAIN
            font_size: 12,
            title: String::new(),
            text: String::new(),
            window_id: None,
            image_id: None,
            resizable: is_window,
            opaque: true,
            cursor_type: 0, // DEFAULT_CURSOR
            minimum_size: (0, 0),
            preferred_size: (0, 0),
            maximum_size: (u32::MAX, u32::MAX),
        }
    }

    /// Walk up the parent chain to compute the absolute screen position.
    pub fn absolute_position(&self, registry: &PeerRegistry) -> (i32, i32) {
        let mut abs_x = self.x;
        let mut abs_y = self.y;
        let mut current_parent = self.parent_id;
        while let Some(pid) = current_parent {
            if let Some(parent) = registry.get(pid) {
                abs_x += parent.x;
                abs_y += parent.y;
                current_parent = parent.parent_id;
            } else {
                break;
            }
        }
        (abs_x, abs_y)
    }

    /// Test whether a point (in the peer's coordinate space) is inside bounds.
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.width as i32 && y < self.height as i32
    }

    /// Returns `true` if this peer represents a top-level window (Frame or Dialog).
    pub fn is_window(&self) -> bool {
        matches!(self.component_type, ComponentType::Frame | ComponentType::Dialog)
    }
}

// ── PeerRegistry ────────────────────────────────────────────────────────

/// Global registry of all live component peers.
pub struct PeerRegistry {
    peers: HashMap<PeerId, ComponentPeer>,
    next_id: u64,
    /// Maps Java object identity hash -> PeerId for reverse lookups.
    java_to_peer: HashMap<i32, PeerId>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            next_id: 1,
            java_to_peer: HashMap::new(),
        }
    }

    /// Allocate a new peer with the given component type and return its ID.
    pub fn create_peer(&mut self, component_type: ComponentType) -> PeerId {
        let id = PeerId(self.next_id);
        self.next_id += 1;
        let peer = ComponentPeer::new(id, component_type);
        self.peers.insert(id, peer);
        id
    }

    /// Look up a peer by ID (immutable).
    pub fn get(&self, id: PeerId) -> Option<&ComponentPeer> {
        self.peers.get(&id)
    }

    /// Look up a peer by ID (mutable).
    pub fn get_mut(&mut self, id: PeerId) -> Option<&mut ComponentPeer> {
        self.peers.get_mut(&id)
    }

    /// Recursively destroy a peer and all its children.
    pub fn destroy(&mut self, id: PeerId) {
        // Collect children first to avoid borrow issues.
        let children: Vec<PeerId> = self
            .peers
            .get(&id)
            .map(|p| p.children.clone())
            .unwrap_or_default();

        // Recursively destroy children.
        for child_id in children {
            self.destroy(child_id);
        }

        // Remove from parent's child list.
        if let Some(peer) = self.peers.get(&id) {
            if let Some(parent_id) = peer.parent_id {
                if let Some(parent) = self.peers.get_mut(&parent_id) {
                    parent.children.retain(|c| *c != id);
                }
            }
        }

        // Remove java mapping entries that point to this peer.
        self.java_to_peer.retain(|_, v| *v != id);

        // Remove the peer itself.
        self.peers.remove(&id);
    }

    /// Establish a parent-child relationship.
    pub fn add_child(&mut self, parent: PeerId, child: PeerId) {
        // Remove child from any existing parent first.
        if let Some(child_peer) = self.peers.get(&child) {
            if let Some(old_parent_id) = child_peer.parent_id {
                if let Some(old_parent) = self.peers.get_mut(&old_parent_id) {
                    old_parent.children.retain(|c| *c != child);
                }
            }
        }

        // Set the new parent on the child.
        if let Some(child_peer) = self.peers.get_mut(&child) {
            child_peer.parent_id = Some(parent);
        }

        // Add the child to the parent's child list.
        if let Some(parent_peer) = self.peers.get_mut(&parent) {
            if !parent_peer.children.contains(&child) {
                parent_peer.children.push(child);
            }
        }
    }

    /// Remove a child from a parent (does not destroy the child).
    pub fn remove_child(&mut self, parent: PeerId, child: PeerId) {
        if let Some(parent_peer) = self.peers.get_mut(&parent) {
            parent_peer.children.retain(|c| *c != child);
        }
        if let Some(child_peer) = self.peers.get_mut(&child) {
            if child_peer.parent_id == Some(parent) {
                child_peer.parent_id = None;
            }
        }
    }

    /// Return all top-level peers (those with no parent).
    pub fn root_peers(&self) -> Vec<PeerId> {
        self.peers
            .values()
            .filter(|p| p.parent_id.is_none())
            .map(|p| p.id)
            .collect()
    }

    /// Hit-test: find the deepest child of `root` that contains the point (x, y)
    /// in the root's coordinate space.
    pub fn find_peer_at(&self, root: PeerId, x: i32, y: i32) -> Option<PeerId> {
        let root_peer = self.peers.get(&root)?;

        // Check if point is within root bounds.
        if !root_peer.contains_point(x - root_peer.x, y - root_peer.y) {
            return None;
        }

        // Check children in reverse order (top-most first, like Z-order).
        for &child_id in root_peer.children.iter().rev() {
            if let Some(child) = self.peers.get(&child_id) {
                if !child.visible {
                    continue;
                }
                // Convert to child's coordinate space.
                let child_x = x - root_peer.x;
                let child_y = y - root_peer.y;
                if let Some(hit) = self.find_peer_at(child_id, child_x, child_y) {
                    return Some(hit);
                }
            }
        }

        // No child was hit, return root itself.
        Some(root)
    }

    /// Register a mapping from a Java object's identity hash code to a peer ID.
    pub fn register_java_mapping(&mut self, java_hash: i32, peer_id: PeerId) {
        self.java_to_peer.insert(java_hash, peer_id);
    }

    /// Look up a peer ID by Java identity hash code.
    pub fn peer_for_java(&self, java_hash: i32) -> Option<PeerId> {
        self.java_to_peer.get(&java_hash).copied()
    }

    /// Return all peer IDs currently in the registry.
    pub fn all_peers(&self) -> Vec<PeerId> {
        self.peers.keys().copied().collect()
    }

    /// Number of live peers.
    pub fn count(&self) -> usize {
        self.peers.len()
    }
}

impl Default for PeerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Global singleton ────────────────────────────────────────────────────

/// Access the global peer registry (locked).
pub fn peer_registry() -> &'static Mutex<PeerRegistry> {
    static INSTANCE: OnceLock<Mutex<PeerRegistry>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(PeerRegistry::new()))
}

// ══════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_peer_assigns_unique_ids() {
        let mut reg = PeerRegistry::new();
        let a = reg.create_peer(ComponentType::Frame);
        let b = reg.create_peer(ComponentType::Panel);
        assert_ne!(a, b);
        assert_eq!(reg.count(), 2);
    }

    #[test]
    fn peer_defaults() {
        let mut reg = PeerRegistry::new();
        let id = reg.create_peer(ComponentType::Frame);
        let peer = reg.get(id).unwrap();
        assert_eq!(peer.width, 640);
        assert_eq!(peer.height, 480);
        assert!(!peer.visible);
        assert!(peer.enabled);
        assert!(peer.resizable);
        assert!(peer.is_window());
    }

    #[test]
    fn panel_defaults() {
        let mut reg = PeerRegistry::new();
        let id = reg.create_peer(ComponentType::Panel);
        let peer = reg.get(id).unwrap();
        assert_eq!(peer.width, 0);
        assert!(!peer.is_window());
        assert!(!peer.resizable);
    }

    #[test]
    fn parent_child() {
        let mut reg = PeerRegistry::new();
        let parent = reg.create_peer(ComponentType::Frame);
        let child = reg.create_peer(ComponentType::Panel);
        reg.add_child(parent, child);

        assert_eq!(reg.get(child).unwrap().parent_id, Some(parent));
        assert!(reg.get(parent).unwrap().children.contains(&child));
    }

    #[test]
    fn remove_child() {
        let mut reg = PeerRegistry::new();
        let parent = reg.create_peer(ComponentType::Frame);
        let child = reg.create_peer(ComponentType::Panel);
        reg.add_child(parent, child);
        reg.remove_child(parent, child);

        assert_eq!(reg.get(child).unwrap().parent_id, None);
        assert!(!reg.get(parent).unwrap().children.contains(&child));
    }

    #[test]
    fn destroy_recursive() {
        let mut reg = PeerRegistry::new();
        let root = reg.create_peer(ComponentType::Frame);
        let child1 = reg.create_peer(ComponentType::Panel);
        let child2 = reg.create_peer(ComponentType::Button);
        let grandchild = reg.create_peer(ComponentType::Label);
        reg.add_child(root, child1);
        reg.add_child(root, child2);
        reg.add_child(child1, grandchild);

        reg.destroy(root);
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn root_peers() {
        let mut reg = PeerRegistry::new();
        let r1 = reg.create_peer(ComponentType::Frame);
        let r2 = reg.create_peer(ComponentType::Dialog);
        let child = reg.create_peer(ComponentType::Panel);
        reg.add_child(r1, child);

        let roots = reg.root_peers();
        assert!(roots.contains(&r1));
        assert!(roots.contains(&r2));
        assert!(!roots.contains(&child));
    }

    #[test]
    fn java_mapping() {
        let mut reg = PeerRegistry::new();
        let id = reg.create_peer(ComponentType::Button);
        reg.register_java_mapping(42, id);

        assert_eq!(reg.peer_for_java(42), Some(id));
        assert_eq!(reg.peer_for_java(99), None);
    }

    #[test]
    fn destroy_clears_java_mapping() {
        let mut reg = PeerRegistry::new();
        let id = reg.create_peer(ComponentType::Button);
        reg.register_java_mapping(42, id);
        reg.destroy(id);

        assert_eq!(reg.peer_for_java(42), None);
    }

    #[test]
    fn contains_point() {
        let mut reg = PeerRegistry::new();
        let id = reg.create_peer(ComponentType::Panel);
        {
            let peer = reg.get_mut(id).unwrap();
            peer.width = 100;
            peer.height = 50;
        }
        let peer = reg.get(id).unwrap();
        assert!(peer.contains_point(0, 0));
        assert!(peer.contains_point(99, 49));
        assert!(!peer.contains_point(100, 0));
        assert!(!peer.contains_point(-1, 0));
    }

    #[test]
    fn absolute_position() {
        let mut reg = PeerRegistry::new();
        let root = reg.create_peer(ComponentType::Frame);
        let child = reg.create_peer(ComponentType::Panel);
        reg.add_child(root, child);
        reg.get_mut(root).unwrap().x = 100;
        reg.get_mut(root).unwrap().y = 200;
        reg.get_mut(child).unwrap().x = 10;
        reg.get_mut(child).unwrap().y = 20;

        let (ax, ay) = reg.get(child).unwrap().absolute_position(&reg);
        assert_eq!(ax, 110);
        assert_eq!(ay, 220);
    }

    #[test]
    fn find_peer_at_basic() {
        let mut reg = PeerRegistry::new();
        let root = reg.create_peer(ComponentType::Frame);
        {
            let p = reg.get_mut(root).unwrap();
            p.x = 0;
            p.y = 0;
            p.width = 200;
            p.height = 200;
            p.visible = true;
        }

        let child = reg.create_peer(ComponentType::Button);
        {
            let p = reg.get_mut(child).unwrap();
            p.x = 50;
            p.y = 50;
            p.width = 100;
            p.height = 30;
            p.visible = true;
        }
        reg.add_child(root, child);

        // Hit the button
        let hit = reg.find_peer_at(root, 60, 55);
        assert_eq!(hit, Some(child));

        // Hit the frame (not the button)
        let hit = reg.find_peer_at(root, 10, 10);
        assert_eq!(hit, Some(root));
    }

    #[test]
    fn reparent_child() {
        let mut reg = PeerRegistry::new();
        let p1 = reg.create_peer(ComponentType::Frame);
        let p2 = reg.create_peer(ComponentType::Frame);
        let child = reg.create_peer(ComponentType::Panel);

        reg.add_child(p1, child);
        assert!(reg.get(p1).unwrap().children.contains(&child));

        // Reparent to p2 -- add_child should remove from p1 automatically.
        reg.add_child(p2, child);
        assert!(!reg.get(p1).unwrap().children.contains(&child));
        assert!(reg.get(p2).unwrap().children.contains(&child));
        assert_eq!(reg.get(child).unwrap().parent_id, Some(p2));
    }

    #[test]
    fn singleton_accessible() {
        let reg = peer_registry();
        let _lock = reg.lock();
    }

    #[test]
    fn all_peers() {
        let mut reg = PeerRegistry::new();
        let a = reg.create_peer(ComponentType::Frame);
        let b = reg.create_peer(ComponentType::Panel);
        let all = reg.all_peers();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&a));
        assert!(all.contains(&b));
    }
}
