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
    reg.register_java_mapping(0, 42, id);

    assert_eq!(reg.peer_for_java(0, 42), Some(id));
    assert_eq!(reg.peer_for_java(0, 99), None);
}

/// gc-common w23-a: two VMs' components with the same identity hash map
/// to distinct peers, and one VM's teardown destroys its peers (with their
/// subtrees) and nobody else's.
#[test]
fn w23a_java_mapping_is_per_vm_and_forgotten_with_the_vm() {
    const VM_A: usize = 0x0A23_0A08;
    const VM_B: usize = 0x0A23_0B08;
    const HASH: i32 = 0x0A23;
    let mut reg = PeerRegistry::new();
    let pa = reg.create_peer(ComponentType::Frame);
    let pa_child = reg.create_peer(ComponentType::Button);
    let pb = reg.create_peer(ComponentType::Frame);
    reg.register_java_mapping(VM_A, HASH, pa);
    reg.register_java_mapping(VM_B, HASH, pb);
    reg.add_child(pa, pa_child);
    assert_eq!(reg.peer_for_java(VM_A, HASH), Some(pa));
    assert_eq!(reg.peer_for_java(VM_B, HASH), Some(pb));
    assert_ne!(pa, pb, "a hash collision across VMs must not share a peer");

    let mut gone = reg.forget_vm(VM_A);
    gone.sort_unstable_by_key(|p| p.0);
    let mut expected = vec![pa, pa_child];
    expected.sort_unstable_by_key(|p| p.0);
    assert_eq!(gone, expected);
    assert!(reg.get(pa).is_none() && reg.get(pa_child).is_none());
    assert_eq!(reg.peer_for_java(VM_A, HASH), None);
    assert_eq!(reg.peer_for_java(VM_B, HASH), Some(pb));
    assert!(reg.get(pb).is_some());
    assert!(reg.forget_vm(VM_A).is_empty(), "idempotent");
}

/// gc-common w24-b: two live components of ONE VM with the same identity
/// hash keep two peers (the second is a collided mapping); destroying the
/// primary promotes the collided one, and `destroy_one` leaves a live
/// child's peer in place (detached).
#[test]
fn w24b_colliding_java_mappings_keep_their_own_peers() {
    const VM: usize = 0x0B24_0A08;
    const HASH: i32 = 0x0B24;
    let mut reg = PeerRegistry::new();
    let first = reg.create_peer(ComponentType::Frame);
    let second = reg.create_peer(ComponentType::Frame);
    let child = reg.create_peer(ComponentType::Button);
    reg.register_java_mapping(VM, HASH, first);
    reg.register_colliding_java_mapping(VM, HASH, second);
    reg.add_child(first, child);
    assert_eq!(
        reg.peer_for_java(VM, HASH),
        Some(first),
        "the primary is kept"
    );
    assert_eq!(reg.collided_peers_for_java(VM, HASH), vec![second]);
    assert!(reg.collided_peers_for_java(VM, HASH + 1).is_empty());
    // Re-filing the primary as a collision is a no-op.
    reg.register_colliding_java_mapping(VM, HASH, first);
    assert_eq!(reg.collided_peers_for_java(VM, HASH), vec![second]);

    assert!(reg.destroy_one(first));
    assert!(!reg.destroy_one(first), "already gone");
    assert_eq!(reg.peer_for_java(VM, HASH), Some(second), "promoted");
    assert!(reg.collided_peers_for_java(VM, HASH).is_empty());
    assert_eq!(
        reg.get(child).map(|c| c.parent_id),
        Some(None),
        "child detached, kept"
    );

    // A teardown still reaches every peer the VM mapped.
    reg.register_colliding_java_mapping(VM, HASH, child);
    let mut gone = reg.forget_vm(VM);
    gone.sort_unstable_by_key(|p| p.0);
    let mut expected = vec![second, child];
    expected.sort_unstable_by_key(|p| p.0);
    assert_eq!(gone, expected);
    assert_eq!(reg.peer_for_java(VM, HASH), None);
    assert!(reg.collided_peers_for_java(VM, HASH).is_empty());
    assert_eq!(reg.count(), 0);
}

#[test]
fn destroy_clears_java_mapping() {
    let mut reg = PeerRegistry::new();
    let id = reg.create_peer(ComponentType::Button);
    reg.register_java_mapping(0, 42, id);
    reg.destroy(id);

    assert_eq!(reg.peer_for_java(0, 42), None);
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
