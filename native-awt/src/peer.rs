// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use std::sync::OnceLock;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

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
    pub background: u32, // ARGB
    pub foreground: u32, // ARGB
    pub font_family: String,
    pub font_style: i32,
    pub font_size: i32,
    pub title: String,          // for Frame/Dialog
    pub text: String,           // for Button/Label/TextField
    pub window_id: Option<u64>, // platform WindowId, only for Frame/Dialog
    pub image_id: Option<u64>,  // backing BufferedImage for double-buffering
    pub resizable: bool,
    pub opaque: bool,
    pub cursor_type: i32, // java.awt.Cursor.DEFAULT_CURSOR etc.
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
        let mut abs_x = self.x as i64;
        let mut abs_y = self.y as i64;
        let mut current_parent = self.parent_id;
        while let Some(pid) = current_parent {
            if let Some(parent) = registry.get(pid) {
                abs_x += parent.x as i64;
                abs_y += parent.y as i64;
                current_parent = parent.parent_id;
            } else {
                break;
            }
        }
        (
            abs_x.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            abs_y.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        )
    }

    /// Test whether a point (in the peer's coordinate space) is inside bounds.
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        self.contains_point_i64(x as i64, y as i64)
    }

    fn contains_point_i64(&self, x: i64, y: i64) -> bool {
        x >= 0 && y >= 0 && x < self.width as i64 && y < self.height as i64
    }

    /// Returns `true` if this peer represents a top-level window (Frame or Dialog).
    pub fn is_window(&self) -> bool {
        matches!(
            self.component_type,
            ComponentType::Frame | ComponentType::Dialog
        )
    }
}

// ── PeerRegistry ────────────────────────────────────────────────────────

/// Global registry of all live component peers.
///
/// Storage uses [`FxHashMap`] throughout: peer IDs are sequential `u64`s and
/// Java identity hashes are already-hashed `i32`s, so SipHash buys nothing
/// over the much cheaper FxHash.
///
/// TODO(eviction): `peers` and the two java<->peer maps grow without bound.
/// If the Java side fails to call `dispose()` (e.g. a missed finalizer or a
/// dropped weak ref on the Java GC side) the entries leak. A weak-reference
/// / liveness-sweep scheme is out of scope for this patch and tracked
/// separately.
pub struct PeerRegistry {
    peers: FxHashMap<PeerId, ComponentPeer>,
    next_id: u64,
    /// Maps Java object identity hash -> PeerId for reverse lookups.
    java_to_peer: FxHashMap<i32, PeerId>,
    /// Reverse of `java_to_peer`: PeerId -> Java identity hash. Lets
    /// `destroy()` clean up the java mapping in O(1) instead of doing a
    /// full `retain` walk (which made destroy() O(n^2) over a peer tree).
    peer_to_java: FxHashMap<PeerId, i32>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self {
            peers: FxHashMap::default(),
            next_id: 1,
            java_to_peer: FxHashMap::default(),
            peer_to_java: FxHashMap::default(),
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

    /// Destroy a peer and all its descendants.
    ///
    /// Uses an explicit heap-allocated work-stack rather than recursion so a
    /// deeply nested Component tree cannot overflow the native thread stack.
    /// The same set of peers is torn down with the same per-peer operations
    /// as the previous recursive implementation.
    pub fn destroy(&mut self, id: PeerId) {
        // Gather the whole subtree (root + all descendants) via an explicit
        // stack walk. Each visited peer is recorded so we can tear it down
        // afterward regardless of traversal order.
        let mut to_destroy: Vec<PeerId> = Vec::new();
        let mut work: Vec<PeerId> = vec![id];
        while let Some(cur) = work.pop() {
            if let Some(peer) = self.peers.get(&cur) {
                work.extend(peer.children.iter().copied());
                to_destroy.push(cur);
            }
        }

        for victim in to_destroy {
            // Remove from parent's child list.
            if let Some(peer) = self.peers.get(&victim) {
                if let Some(parent_id) = peer.parent_id {
                    if let Some(parent) = self.peers.get_mut(&parent_id) {
                        parent.children.retain(|c| *c != victim);
                    }
                }
            }

            // Remove java mapping entries that point to this peer in O(1) via
            // the reverse map. Previously this used `java_to_peer.retain(...)`,
            // which is O(map size) per destroyed peer -- making destroy() of a
            // tree O(n^2). Now it's O(1) per peer, so the whole tree teardown
            // is O(n).
            if let Some(java_hash) = self.peer_to_java.remove(&victim) {
                self.java_to_peer.remove(&java_hash);
            }

            // Remove the peer itself.
            self.peers.remove(&victim);
        }
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
        // Iterative depth-first hit-test using an explicit heap stack so a
        // deeply nested Component tree cannot overflow the native thread
        // stack. Each stack entry holds a peer and the query point expressed
        // in that peer's parent coordinate space — exactly the (id, x, y)
        // triple the recursive version passed down.
        //
        // The recursion never backtracks to a sibling once a child contains
        // the point, so the walk is a single descending path: at each level
        // we follow the first (top-most, via `children.iter().rev()`) visible
        // child that contains the point. The stack therefore holds at most
        // one pending entry, but using a heap Vec keeps depth heap-bounded.
        let mut stack: Vec<(PeerId, i64, i64)> = vec![(root, x as i64, y as i64)];

        while let Some((id, px, py)) = stack.pop() {
            let peer = match self.peers.get(&id) {
                Some(p) => p,
                None => continue,
            };

            // Check if point is within this peer's bounds.
            if !peer.contains_point_i64(px - peer.x as i64, py - peer.y as i64) {
                continue;
            }

            // Point is inside this peer. Convert to child coordinate space and
            // push visible children. If none yield a hit, the deepest peer that
            // contains the point is returned, preserving the recursive
            // semantics: a peer is returned only after all its (top-most-first)
            // children have failed to contain the point.
            let child_x = px - peer.x as i64;
            let child_y = py - peer.y as i64;

            // The recursive version returns the FIRST top-most child whose
            // subtree contains the point, and never backtracks to a sibling
            // once such a child is found. So we descend along a single path:
            // pick the first (top-most) visible child that contains the point
            // and continue from there; if none does, this peer is the answer.
            let mut descended = false;
            for &child_id in peer.children.iter().rev() {
                if let Some(child) = self.peers.get(&child_id) {
                    if !child.visible {
                        continue;
                    }
                    if child.contains_point_i64(child_x - child.x as i64, child_y - child.y as i64)
                    {
                        stack.push((child_id, child_x, child_y));
                        descended = true;
                        break;
                    }
                }
            }

            if !descended {
                // No child was hit; this peer is the deepest match.
                return Some(id);
            }
        }

        None
    }

    /// Register a mapping from a Java object's identity hash code to a peer ID.
    ///
    /// Maintains both `java_to_peer` and the reverse `peer_to_java` map so
    /// that [`destroy`] can tear down the mapping in O(1). If `peer_id` was
    /// previously associated with a different java hash, that stale forward
    /// entry is removed first; likewise if `java_hash` was previously bound
    /// to a different peer, that stale reverse entry is removed.
    pub fn register_java_mapping(&mut self, java_hash: i32, peer_id: PeerId) {
        // Drop any prior forward mapping for this peer.
        if let Some(old_hash) = self.peer_to_java.insert(peer_id, java_hash) {
            if old_hash != java_hash {
                self.java_to_peer.remove(&old_hash);
            }
        }
        // Drop any prior reverse mapping for this java hash.
        if let Some(old_peer) = self.java_to_peer.insert(java_hash, peer_id) {
            if old_peer != peer_id {
                self.peer_to_java.remove(&old_peer);
            }
        }
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
    fn absolute_position_saturates_extreme_coordinates() {
        let mut reg = PeerRegistry::new();
        let root = reg.create_peer(ComponentType::Frame);
        let child = reg.create_peer(ComponentType::Panel);
        reg.add_child(root, child);
        reg.get_mut(root).unwrap().x = i32::MAX;
        reg.get_mut(root).unwrap().y = i32::MIN;
        reg.get_mut(child).unwrap().x = 100;
        reg.get_mut(child).unwrap().y = -100;

        let (ax, ay) = reg.get(child).unwrap().absolute_position(&reg);
        assert_eq!(ax, i32::MAX);
        assert_eq!(ay, i32::MIN);
    }

    #[test]
    fn contains_point_handles_large_dimensions() {
        let mut reg = PeerRegistry::new();
        let id = reg.create_peer(ComponentType::Panel);
        {
            let peer = reg.get_mut(id).unwrap();
            peer.width = u32::MAX;
            peer.height = u32::MAX;
        }
        let peer = reg.get(id).unwrap();
        assert!(peer.contains_point(i32::MAX, i32::MAX));
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
