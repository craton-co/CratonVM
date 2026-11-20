// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
/// Eviction: no native destroys a peer on `removeNotify` / `dispose` (neither
/// is registered). gc-common w24-b: `natives.rs::sweep_dead_awt_rows` destroys
/// the peer of a component the collector found dead ([`Self::destroy_one`]),
/// judged by the WEAK root its peer-source row holds; a VM's teardown destroys
/// the rest ([`Self::forget_vm`]).
pub struct PeerRegistry {
    peers: FxHashMap<PeerId, ComponentPeer>,
    next_id: u64,
    /// Maps `(vm, Java object identity hash)` -> PeerId for reverse lookups.
    ///
    /// gc-common w23-a (`common-w22a-awt-state-is-process-wide-and-keyed-by-identity-hash`,
    /// item 2): the key used to be the bare identity hash, so two VMs'
    /// components whose hashes collided shared ONE peer (bounds, visibility,
    /// dirty state). The VM identity is now part of the key; peer ids stay
    /// process-unique, so every other peer-keyed table is per VM by content.
    java_to_peer: FxHashMap<JavaKey, PeerId>,
    /// Reverse of `java_to_peer`: PeerId -> `(vm, hash)`. Lets `destroy()`
    /// clean up the java mapping in O(1) instead of doing a full `retain`
    /// walk (which made destroy() O(n^2) over a peer tree), and names the VM
    /// that owns each mapped peer ([`Self::forget_vm`]). Covers the
    /// `java_collided` peers too.
    peer_to_java: FxHashMap<PeerId, JavaKey>,
    /// gc-common w24-b: further peers of a key whose `java_to_peer` peer
    /// belongs to ANOTHER live component of the same VM (a within-VM
    /// identity-hash collision, told apart by the peer-source rows' weak
    /// roots in `natives.rs`). Almost always empty.
    java_collided: FxHashMap<JavaKey, Vec<PeerId>>,
}

/// `(NativeContext::vm_identity(), identity hash)` of a Java component.
pub type JavaKey = (usize, i32);

impl PeerRegistry {
    pub fn new() -> Self {
        Self {
            peers: FxHashMap::default(),
            next_id: 1,
            java_to_peer: FxHashMap::default(),
            peer_to_java: FxHashMap::default(),
            java_collided: FxHashMap::default(),
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
        let _ = self.destroy_subtree(id);
    }

    /// [`Self::destroy`], returning the ids it removed.
    fn destroy_subtree(&mut self, id: PeerId) -> Vec<PeerId> {
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

        for &victim in &to_destroy {
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
            self.unlink_java(victim);

            // Remove the peer itself.
            self.peers.remove(&victim);
        }
        to_destroy
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
    pub fn register_java_mapping(&mut self, vm: usize, java_hash: i32, peer_id: PeerId) {
        let key = (vm, java_hash);
        if self.java_to_peer.get(&key) == Some(&peer_id) {
            return;
        }
        // Drop any prior mapping of this peer (primary or collided).
        self.unlink_java(peer_id);
        // Drop any prior reverse mapping for this java key.
        if let Some(old_peer) = self.java_to_peer.insert(key, peer_id) {
            if old_peer != peer_id {
                self.peer_to_java.remove(&old_peer);
            }
        }
        self.peer_to_java.insert(peer_id, key);
    }

    /// gc-common w24-b: map `peer_id` as a FURTHER peer of `(vm, java_hash)`,
    /// whose primary peer belongs to another live component (a within-VM
    /// identity-hash collision). The primary mapping is kept. Without a
    /// primary this is [`Self::register_java_mapping`].
    pub fn register_colliding_java_mapping(&mut self, vm: usize, java_hash: i32, peer_id: PeerId) {
        let key = (vm, java_hash);
        match self.java_to_peer.get(&key) {
            None => return self.register_java_mapping(vm, java_hash, peer_id),
            Some(&primary) if primary == peer_id => return,
            Some(_) => {}
        }
        self.unlink_java(peer_id);
        self.java_collided.entry(key).or_default().push(peer_id);
        self.peer_to_java.insert(peer_id, key);
    }

    /// Remove `peer`'s java mapping, primary or collided. When the primary
    /// goes, the key's first collided peer (if any) becomes the primary.
    fn unlink_java(&mut self, peer: PeerId) {
        let Some(key) = self.peer_to_java.remove(&peer) else {
            return;
        };
        if self.java_to_peer.get(&key) == Some(&peer) {
            self.java_to_peer.remove(&key);
            if let Some(mut rest) = self.java_collided.remove(&key) {
                if !rest.is_empty() {
                    let next = rest.remove(0);
                    self.java_to_peer.insert(key, next);
                }
                if !rest.is_empty() {
                    self.java_collided.insert(key, rest);
                }
            }
        } else if let Some(rest) = self.java_collided.get_mut(&key) {
            rest.retain(|&p| p != peer);
            if rest.is_empty() {
                self.java_collided.remove(&key);
            }
        }
    }

    /// Look up the peer VM `vm` registered for a Java identity hash code (the
    /// primary one; see [`Self::collided_peers_for_java`]).
    pub fn peer_for_java(&self, vm: usize, java_hash: i32) -> Option<PeerId> {
        self.java_to_peer.get(&(vm, java_hash)).copied()
    }

    /// gc-common w24-b: the further peers of `(vm, java_hash)` filed by
    /// [`Self::register_colliding_java_mapping`] (empty -- no allocation --
    /// in the common case).
    pub fn collided_peers_for_java(&self, vm: usize, java_hash: i32) -> Vec<PeerId> {
        self.java_collided
            .get(&(vm, java_hash))
            .cloned()
            .unwrap_or_default()
    }

    /// gc-common w24-b: destroy ONE peer -- the peer of a component the
    /// collector found dead. Unlike [`Self::destroy`] its children are
    /// detached (parent `None`), not destroyed: a child may still be alive.
    /// Returns `false` if the peer is unknown.
    pub fn destroy_one(&mut self, id: PeerId) -> bool {
        let Some(peer) = self.peers.remove(&id) else {
            return false;
        };
        if let Some(parent_id) = peer.parent_id {
            if let Some(parent) = self.peers.get_mut(&parent_id) {
                parent.children.retain(|c| *c != id);
            }
        }
        for child in peer.children {
            if let Some(c) = self.peers.get_mut(&child) {
                if c.parent_id == Some(id) {
                    c.parent_id = None;
                }
            }
        }
        self.unlink_java(id);
        true
    }

    /// VM teardown (gc-common w23-a): destroy every peer VM `vm` mapped, with
    /// its subtree and java mappings, and return the destroyed ids (so the
    /// caller can drop the peer-keyed rows of other tables). Peers of other
    /// VMs are untouched. Idempotent.
    pub fn forget_vm(&mut self, vm: usize) -> Vec<PeerId> {
        let roots: Vec<PeerId> = self
            .peer_to_java
            .iter()
            .filter(|(_, key)| key.0 == vm)
            .map(|(&id, _)| id)
            .collect();
        let mut destroyed = Vec::new();
        for id in roots {
            destroyed.extend(self.destroy_subtree(id));
        }
        destroyed
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
mod tests;
