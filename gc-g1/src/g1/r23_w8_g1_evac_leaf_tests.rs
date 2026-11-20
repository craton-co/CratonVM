use super::*;

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

const CHAINS: usize = 64;
const DEPTH: usize = 60;
const PAYLOAD: usize = 24;

fn keep_long_pause_keeps_every_node_and_payload(label: &str) {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 32 * 1024 * 1024,
        initial_heap_size: 32 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 4,
        ..G1CollectorConfig::default()
    });
    let ring = gc.alloc_object(ClassId::new(1), CHAINS);
    // Pre-pause byte values and payload addresses, per (chain, depth).
    let mut want: Vec<Vec<Value>> = Vec::with_capacity(CHAINS * DEPTH);
    let mut before: Vec<usize> = Vec::with_capacity(CHAINS * DEPTH);
    for c in 0..CHAINS {
        for d in 0..DEPTH {
            let tag = c * DEPTH + d;
            let node = gc.alloc_object(ClassId::new(1), 3);
            let payload = gc.alloc_array(ClassId::new(0), ArrayElementType::Byte, PAYLOAD);
            for i in 0..PAYLOAD {
                gc.set_array_element(payload, i, Value::Int(((tag + i) % 127) as i32))
                    .expect("byte store");
            }
            want.push(
                (0..PAYLOAD)
                    .map(|i| gc.get_array_element(payload, i).expect("byte load"))
                    .collect(),
            );
            before.push(payload.as_ptr() as usize);
            let prev = gc.get_field(ring, c);
            gc.set_field(node, 0, prev);
            gc.set_field(node, 1, Value::Object(Some(payload)));
            gc.set_field(node, 2, Value::Int(tag as i32));
            gc.set_field(ring, c, Value::Object(Some(node)));
        }
    }
    let mut roots: Vec<ObjectRef> = vec![ring];
    let _ = gc.young_collection_parallel(&mut roots, &NoopMonitors);
    let ring = roots[0];
    let mut seen = 0usize;
    let mut moved = 0usize;
    for c in 0..CHAINS {
        let mut cur = gc.get_field(ring, c);
        let mut d = DEPTH;
        while let Value::Object(Some(node)) = cur {
            assert!(d > 0, "{label}: chain {c} has more than {DEPTH} nodes");
            d -= 1;
            let tag = c * DEPTH + d;
            assert_eq!(
                gc.get_field(node, 2),
                Value::Int(tag as i32),
                "{label}: chain {c}: node {d} is the one that was linked there"
            );
            let Value::Object(Some(payload)) = gc.get_field(node, 1) else {
                panic!("{label}: chain {c} node {d} lost its payload reference");
            };
            if payload.as_ptr() as usize != before[tag] {
                moved += 1;
            }
            let got: Vec<Value> = (0..PAYLOAD)
                .map(|i| gc.get_array_element(payload, i).expect("byte load"))
                .collect();
            assert_eq!(got, want[tag], "{label}: chain {c} node {d}: payload bytes");
            seen += 1;
            cur = gc.get_field(node, 0);
        }
        assert_eq!(d, 0, "{label}: chain {c} lost {d} node(s)");
    }
    assert_eq!(seen, CHAINS * DEPTH, "{label}");
    // The payloads were young and the pause copied them: a leaf that is
    // not pushed is still evacuated and its slot still rewritten.
    assert_eq!(moved, CHAINS * DEPTH, "{label}: every payload was copied");
}


#[test]
fn r23_w8_the_leaf_test_is_primitive_arrays_only() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..G1CollectorConfig::default()
    });
    let header = |r: ObjectRef| unsafe { &*(r.as_ptr() as *const ObjectHeader) };
    for et in [
        ArrayElementType::Boolean,
        ArrayElementType::Char,
        ArrayElementType::Float,
        ArrayElementType::Double,
        ArrayElementType::Byte,
        ArrayElementType::Short,
        ArrayElementType::Int,
        ArrayElementType::Long,
    ] {
        let a = gc.alloc_array(ClassId::new(0), et, 4);
        assert!(evac_header_is_primitive_array(header(a)), "{et:?}[] is a leaf");
    }
    let refs = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 4);
    assert!(!evac_header_is_primitive_array(header(refs)), "Object[] is scanned");
    let obj = gc.alloc_object(ClassId::new(1), 2);
    assert!(!evac_header_is_primitive_array(header(obj)), "an object is scanned");
}
