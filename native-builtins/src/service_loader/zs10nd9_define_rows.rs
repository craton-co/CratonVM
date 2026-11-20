use super::*;
use crate::test_utils::{
    assert_kill_row, assert_reserved_row, spent_define_row, MockNativeContext,
};
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

/// `public class <name> extends java/lang/Object {}` (version 52).
fn class_bytes(name: &str) -> Vec<u8> {
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&5u16.to_be_bytes());
    for (index, text) in [(2u16, name), (4u16, "java/lang/Object")] {
        b.push(7);
        b.extend_from_slice(&index.to_be_bytes());
        b.push(1);
        b.extend_from_slice(&(text.len() as u16).to_be_bytes());
        b.extend_from_slice(text.as_bytes());
    }
    b.extend_from_slice(&0x0021u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&3u16.to_be_bytes());
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    b
}

fn row(kill: bool) -> crate::test_utils::SpentDefineRow {
    let mut ctx = MockNativeContext::new();
    let simple = if kill { "ImplK" } else { "Impl" };
    let name = format!("org/elasticsearch/core/zs10nd9/{simple}");
    ctx.set_resource("IMPL-JARS/core/LISTING.TXT", b"zs10nd9.jar\n".to_vec());
    ctx.set_resource(
        &format!("IMPL-JARS/core/zs10nd9.jar/{name}.class"),
        class_bytes(&name),
    );
    let cid = ctx.ensure_class_initialized("java/lang/Object").unwrap();
    let loader = ctx.alloc_object(cid, 1);
    spent_define_row(&mut ctx, kill, |ctx| {
        let out = impl_jars_load_class(ctx, Some(loader), &name);
        // Served = a mirror; a define that found nothing answers Ok(None).
        out.map(|m| m.map(|m| Value::Object(Some(m))))
    })
}

#[test]
fn zs10nd9_impl_jars_define_fails_before_it_publishes() {
    assert_reserved_row("impl_jars_load_class_inner", &row(false));
}

#[test]
fn zs10nd9_impl_jars_define_kill_defines_first() {
    assert_kill_row("impl_jars_load_class_inner", &row(true));
}
