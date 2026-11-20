use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

fn pool() -> ConstantPool {
    ConstantPool::new(vec![
        E::Tombstone,
        E::Utf8("conf21-abc".into()),
        E::StringReference { string_index: 1 },
        E::Integer(5),
        E::Utf8("conf21-x".into()),
        E::StringReference { string_index: 4 },
    ])
}

/// `ldc #2`, `ldc #3` (an int), `ldc_w #5`, `ldc #2` again, `ldc #9` (no
/// entry), `areturn`: the String sites once each, in pool-index order.
const CODE: [u8; 12] = [0x12, 2, 0x12, 3, 0x13, 0, 5, 0x12, 2, 0x12, 9, 0xb0];

#[test]
fn the_scan_names_each_string_site_once() {
    assert_eq!(
        string_ldc_sites(&pool(), Some(&CODE)),
        vec![(2, "conf21-abc".to_string()), (5, "conf21-x".to_string())]
    );
    assert_eq!(
        string_ldc_sites(&pool(), Some(&[0x13, 0, 5, 0xb0])),
        vec![(5, "conf21-x".to_string())]
    );
    assert!(string_ldc_sites(&pool(), Some(&[0x12])).is_empty(), "truncated operand");
    assert!(string_ldc_sites(&pool(), Some(&[0x03, 0xac])).is_empty(), "no ldc");
    // The whole pool: both Strings, whatever the code loads.
    assert_eq!(
        string_ldc_sites(&pool(), None),
        vec![(2, "conf21-abc".to_string()), (5, "conf21-x".to_string())]
    );
}

/// After the eager pass the site's record holds the pooled literal, so the
/// `ldc` pushes that object, and `String.intern()` of an equal String finds
/// it pooled; a second pass resolves nothing.
#[test]
fn the_eager_pass_records_the_pooled_literal_the_ldc_then_pushes() {
    let mut config = VmConfig::default();
    config.ldc_const_cache = true;
    let shared = SharedVm::new(config);
    let class_id = {
        let mut cm = shared.classes.class_manager.write();
        let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/conf21/EagerLdc", 0) else {
            return; // --real-jdk policy in the environment: nothing to fabricate
        };
        let Some(class) = cm.class_store.get_mut(id) else {
            return;
        };
        class.constant_pool = pool();
        id
    };
    assert!(probe_recorded_cp_constant(&shared, class_id, 2).is_none());
    assert!(shared.mem.string_pool.read().get("conf21-abc").is_none());
    // The method's sites: `ldc_w #5` only.
    assert_eq!(resolve_string_ldcs_eagerly(&shared, class_id, Some(&[0x13, 0, 5, 0xb0])), 1);
    assert!(probe_recorded_cp_constant(&shared, class_id, 2).is_none());
    // The whole pool: the rest.
    assert_eq!(resolve_string_ldcs_eagerly(&shared, class_id, None), 1);
    let pooled = shared
        .mem
        .string_pool
        .read()
        .get("conf21-abc")
        .copied()
        .expect("the literal is pooled");
    assert!(matches!(
        probe_recorded_cp_constant(&shared, class_id, 2),
        Some(Value::Object(Some(o))) if o == pooled
    ));
    assert!(probe_recorded_cp_constant(&shared, class_id, 5).is_some());
    assert_eq!(resolve_string_ldcs_eagerly(&shared, class_id, Some(&CODE)), 0);
    assert_eq!(resolve_string_ldcs_eagerly(&shared, class_id, None), 0);

    let mut thread = JvmThread::new(ThreadId(0), "conf21");
    thread.frames.push(Frame::new(
        class_id,
        "cratonvm/test/conf21/EagerLdc".to_string(),
        "m".to_string(),
        "()Ljava/lang/String;".to_string(),
        None,
        CODE.to_vec(),
        vec![],
        4,
        1,
        &[],
    ));
    assert!(execute_ldc(&shared, &mut thread, 0, 2).is_ok());
    assert!(matches!(
        thread.frames[0].stack.pop(),
        Ok(Value::Object(Some(o))) if o == pooled
    ));
}

/// An absent class resolves nothing and does not panic.
#[test]
fn an_absent_class_resolves_nothing() {
    let shared = SharedVm::new(VmConfig::default());
    assert_eq!(
        resolve_string_ldcs_eagerly(&shared, ClassId::new(61_998), Some(&CODE)),
        0
    );
}
