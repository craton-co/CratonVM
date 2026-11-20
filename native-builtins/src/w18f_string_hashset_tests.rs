use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeHeapAccess};

fn declare(ctx: &MockNativeContext, class: cratonvm_types::ClassId, names: &[&str]) {
    let fields = names
        .iter()
        .enumerate()
        .map(|(slot, name)| FieldMetadata {
            name: name.to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: class,
            is_static: false,
        })
        .collect();
    ctx.set_declared_fields(class, fields);
}

/// gc-common w18-f handoff
/// (`handoff-w18f-string-hashset-builder-roots-its-keys`): the real-layout
/// build roots its keys, buckets and map for its whole run and gives every
/// pin back, and each key is reachable through the set it built.
#[test]
fn the_real_layout_string_hashset_releases_its_pins_and_holds_every_key() {
    let mut ctx = MockNativeContext::new();
    let map_cid = ctx.ensure_class_initialized("java/util/HashMap").unwrap();
    let node_cid = ctx
        .ensure_class_initialized("java/util/HashMap$Node")
        .unwrap();
    let set_cid = ctx.ensure_class_initialized("java/util/HashSet").unwrap();
    declare(
        &ctx,
        map_cid,
        &["table", "size", "threshold", "loadFactor", "entrySet"],
    );
    declare(&ctx, node_cid, &["hash", "key", "value", "next"]);
    declare(&ctx, set_cid, &["map"]);
    let names = ["acl", "basic", "dos", "owner", "posix"];
    let keys: Vec<ObjectRef> = names.iter().map(|n| ctx.create_string(n)).collect();
    let pins_before = ctx.native_pin_count_for_test();

    let set = build_real_layout_string_hashset(&mut ctx, &keys).expect("built");

    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins_before,
        "every pin released"
    );
    let Value::Object(Some(map)) = ctx.get_field(set, 0) else {
        panic!("the set holds its map");
    };
    let Value::Object(Some(buckets)) = ctx.get_field(map, 0) else {
        panic!("the map holds its table");
    };
    let mut found: Vec<String> = Vec::new();
    for idx in 0..16 {
        let mut cursor = ctx.get_array_element(buckets, idx);
        while let Value::Object(Some(node)) = cursor {
            if let Value::Object(Some(key)) = ctx.get_field(node, 1) {
                found.push(ctx.read_string(key).unwrap_or_default());
            }
            cursor = ctx.get_field(node, 3);
        }
    }
    found.sort();
    assert_eq!(found, names.map(str::to_string).to_vec());
}
