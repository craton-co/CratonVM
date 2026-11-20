use super::*;
use crate::test_utils::{
    assert_kill_row, assert_retired_kill_row, assert_reserved_row, spent_define_row, MockNativeContext,
};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

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

fn byte_array(ctx: &mut MockNativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    arr
}

fn lk_define_class_row(kill: bool) -> crate::test_utils::SpentDefineRow {
    let mut ctx = MockNativeContext::new();
    let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();
    let arr = byte_array(&mut ctx, &class_bytes("zs10nd9/LkDefine"));
    spent_define_row(&mut ctx, kill, |ctx| {
        lk_define_class(
            ctx,
            &[Value::Object(Some(lookup)), Value::Object(Some(arr))],
        )
    })
}

#[test]
fn zs10nd9_lk_define_class_fails_before_it_publishes() {
    assert_reserved_row("lk_define_class", &lk_define_class_row(false));
}

#[test]
fn zs10nd9_lk_define_class_retired_kill_still_reserves() {
    assert_retired_kill_row("lk_define_class", &lk_define_class_row(true));
}

fn lk_define_hidden_class_row(kill: bool) -> crate::test_utils::SpentDefineRow {
    let mut ctx = MockNativeContext::new();
    let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();
    let arr = byte_array(&mut ctx, &class_bytes("zs10nd9/LkHidden"));
    spent_define_row(&mut ctx, kill, |ctx| {
        lk_define_hidden_class(
            ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Object(None),
            ],
        )
    })
}

#[test]
fn zs10nd9_lk_define_hidden_class_fails_before_it_publishes() {
    assert_reserved_row("lk_define_hidden_class", &lk_define_hidden_class_row(false));
}

#[test]
fn zs10nd9_lk_define_hidden_class_retired_kill_still_reserves() {
    assert_retired_kill_row("lk_define_hidden_class", &lk_define_hidden_class_row(true));
}

/// A synthetic-layout `URLClassLoader` whose one URL is a directory
/// holding `zs10nd9/<simple>.class`.
fn ucl_fixture(ctx: &mut MockNativeContext, simple: &str) -> ObjectRef {
    let dir = std::env::temp_dir().join(format!("zs10nd9-ucl-{}-{simple}", std::process::id()));
    let pkg = dir.join("zs10nd9");
    std::fs::create_dir_all(&pkg).expect("temp class dir");
    std::fs::write(
        pkg.join(format!("{simple}.class")),
        class_bytes(&format!("zs10nd9/{simple}")),
    )
    .expect("class file");
    let loader = ctx.alloc_object(cratonvm_types::ClassId::new(1), UCL_FIELD_COUNT);
    let url = ctx.create_string(&dir.to_string_lossy());
    let urls = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(urls, 0, Value::Object(Some(url)));
    ctx.set_field(loader, UCL_URLS_ARRAY, Value::Object(Some(urls)));
    ctx.set_field(loader, UCL_URL_COUNT, Value::Int(1));
    loader
}

fn ucl_row(kill: bool) -> crate::test_utils::SpentDefineRow {
    let mut ctx = MockNativeContext::new();
    let simple = if kill { "UclK" } else { "Ucl" };
    let loader = ucl_fixture(&mut ctx, simple);
    let name = format!("zs10nd9/{simple}");
    spent_define_row(&mut ctx, kill, |ctx| {
        match ucl_try_define_local_class(ctx, loader, &name) {
            Some(result) => result,
            None => Ok(None),
        }
    })
}

#[test]
fn zs10nd9_ucl_try_define_local_class_fails_before_it_publishes() {
    assert_reserved_row("ucl_try_define_local_class", &ucl_row(false));
}

#[test]
fn zs10nd9_ucl_try_define_local_class_kill_defines_first() {
    assert_kill_row("ucl_try_define_local_class", &ucl_row(true));
}
