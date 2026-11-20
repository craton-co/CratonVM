use super::*;
use cratonvm_types::field_layout::{CompactLayout, FieldStorageKind};

/// A class id no other gc test registers (layouts are process-wide).
const KNOB_CLASS: u32 = 4295;
const CHAINS: usize = 16;
const DEPTH: usize = 40;

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn compact_layout() -> CompactLayout {
    CompactLayout {
        field_disps: vec![8, 16, 24],
        is_ref: vec![true, false, true],
        field_kinds: vec![
            FieldStorageKind::Reference,
            FieldStorageKind::Int,
            FieldStorageKind::Reference,
        ],
        ref_disps: vec![8, 24],
        total_size: 32,
    }
}

/// One serial young pause over a `KeepLong`-shaped graph (chains of
/// compact nodes, each holding the previous node, a tag and an `int[]`,
/// plus a legacy node per link), then the whole graph read back.
fn pause_and_read(overrides: &[(&str, Option<&str>)]) -> (Vec<String>, usize, usize) {
    cratonvm_types::flags::with_thread_overrides(overrides, || {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            initial_heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            ..G1CollectorConfig::default()
        });
        cratonvm_types::register_class_layout(
            gc.layout_domain(),
            KNOB_CLASS,
            std::sync::Arc::new(compact_layout()),
        );
        let ring = gc.alloc_object(ClassId::new(1), CHAINS);
        for c in 0..CHAINS {
            for d in 0..DEPTH {
                let tag = (c * DEPTH + d) as i32;
                let node = gc.try_alloc_object(ClassId::new(KNOB_CLASS), 3).expect("room");
                let payload = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 3 + d % 5);
                gc.set_array_element(payload, 0, Value::Int(tag)).expect("int store");
                let legacy = gc.alloc_object(ClassId::new(1), 2);
                gc.set_field(legacy, 0, Value::Object(Some(payload)));
                gc.set_field(legacy, 1, Value::Int(-tag));
                let prev = gc.get_field(ring, c);
                gc.set_field(node, 0, prev);
                gc.set_field(node, 1, Value::Int(tag));
                gc.set_field(node, 2, Value::Object(Some(legacy)));
                gc.set_field(ring, c, Value::Object(Some(node)));
            }
        }
        let mut roots = vec![ring];
        // Two pauses: the second runs with the first's forward count as
        // its pre-size hint (`CRATONVM_G1_SERIAL_FORWARD_PRESIZE`).
        let first = gc.young_collection_serial(&mut roots, &NoopMonitors);
        let result = gc.young_collection_serial(&mut roots, &NoopMonitors);
        let result = GcResult {
            stats: GcStats {
                objects_copied: first.stats.objects_copied + result.stats.objects_copied,
                bytes_copied: first.stats.bytes_copied + result.stats.bytes_copied,
                ..result.stats
            },
            pointer_map: result.pointer_map,
        };
        let ring = roots[0];
        let mut seen = Vec::new();
        for c in 0..CHAINS {
            let mut cur = gc.get_field(ring, c);
            while let Value::Object(Some(node)) = cur {
                let Value::Object(Some(legacy)) = gc.get_field(node, 2) else {
                    panic!("node lost its legacy child");
                };
                let Value::Object(Some(payload)) = gc.get_field(legacy, 0) else {
                    panic!("legacy child lost its payload");
                };
                seen.push(format!(
                    "{c}:{:?}:{:?}:{:?}:{}:{}",
                    gc.get_field(node, 1),
                    gc.get_field(legacy, 1),
                    gc.get_array_element(payload, 0),
                    object_total_size(header(node)),
                    object_total_size(header(payload)),
                ));
                cur = gc.get_field(node, 0);
            }
        }
        (seen, result.stats.objects_copied, result.stats.bytes_copied)
    })
}

fn header(r: ObjectRef) -> &'static ObjectHeader {
    // SAFETY: read right after the pause, with no collection in between.
    unsafe { &*(r.as_ptr() as *const ObjectHeader) }
}



/// `CRATONVM_G1_SERIAL_FORWARD_PRESIZE`: an empty map with at least the
/// hinted (capped) room; `=0` and a zero hint give the plain default.
#[test]
fn r23_w8_the_serial_forward_map_is_presized_from_the_last_pause() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..G1CollectorConfig::default()
    });
    {
            let m = gc.serial_pointer_map();
            assert!(m.is_empty());
            gc.note_serial_forwards(10_000);
            let m = gc.serial_pointer_map();
            assert!(m.is_empty(), "a capacity is not an entry");
            assert!(m.capacity() >= 10_000, "capacity {}", m.capacity());
            gc.note_serial_forwards(usize::MAX);
            let m = gc.serial_pointer_map();
            assert!(m.capacity() >= SERIAL_FORWARD_PRESIZE_CAP);
            assert!(m.capacity() < SERIAL_FORWARD_PRESIZE_CAP * 4, "the cap holds");        }    }

#[test]
fn r23_w8_the_shape_key_compares_like_the_decoded_shape() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..G1CollectorConfig::default()
    });
    cratonvm_types::register_class_layout(
        gc.layout_domain(),
        KNOB_CLASS,
        std::sync::Arc::new(compact_layout()),
    );
    let objs = [
        gc.try_alloc_object(ClassId::new(KNOB_CLASS), 3).expect("room"),
        gc.try_alloc_object(ClassId::new(KNOB_CLASS), 3).expect("room"),
        gc.alloc_object(ClassId::new(1), 2),
        gc.alloc_object(ClassId::new(1), 3),
        gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 4),
        gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 5),
    ];
    for &a in &objs {
        for &b in &objs {
            let decoded = evac_shape_key(header(a), false) == evac_shape_key(header(b), false);
            let keyed = evac_shape_key(header(a), true) == evac_shape_key(header(b), true);
            assert_eq!(keyed, decoded, "{:?} vs {:?}", a.as_ptr(), b.as_ptr());
        }
    }
}
