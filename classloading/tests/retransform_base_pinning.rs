// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter round i1 wave 28, lane L3: a retransformation base the class
//! path cannot give back is pinned in the class-bytes cache, out of the 16 MiB
//! FIFO's reach
//! (`docs/internal/fixed-bugs/interpreter-L3-an-evicted-retransformation-base-skips-the-class-FIXED-20260929.md`).
//!
//! Until then every base sat in the FIFO. Once a run had defined more class
//! bytes than the cap, the base of a class retransformed late was gone, the
//! class-path fallback found either no file or the untransformed build, and
//! `retransformClasses` skipped the class for the rest of the run.
//!
//! Each test goes through the real insert path (a staged load-time transform
//! loaded by `load_class`, a `redefine_class`), shrinks the FIFO cap, and pushes
//! other bytes through it. Fixtures are shared with `wp2_4b_redefine.rs`.

use cratonvm_classloading::{
    ClassId, ClassLoaderId, ClassManager, DefineClassOptions, RedefineOptions,
};
use std::path::PathBuf;

fn load_fixture(name: &str) -> Option<Vec<u8>> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("wp2_4b_redefine");
    p.push(name);
    std::fs::read(p).ok()
}

fn fixtures() -> Option<(Vec<u8>, Vec<u8>)> {
    Some((load_fixture("Foo.v1.class")?, load_fixture("Foo.v2.class")?))
}

/// Push 40 unrelated 64-byte entries through the FIFO: with a 64-byte cap,
/// everything the FIFO holds from before is evicted.
fn flood_the_fifo(cm: &mut ClassManager) {
    cm.set_class_bytes_cache_cap(64);
    for i in 0..40u32 {
        cm.insert_class_bytes(ClassId::new(900_000 + i), vec![i as u8; 64]);
    }
}

fn cached(cm: &ClassManager, id: ClassId) -> Option<Vec<u8>> {
    cm.class_bytes_cache.get(&id).map(|b| b.to_vec())
}

#[test]
fn an_untransformed_define_is_still_evictable() {
    // The control: a class the class path CAN give back stays in the FIFO,
    // or the pinning below would read as working while it had pinned
    // everything (the memory the FIFO exists to bound).
    let Some((v1, _)) = fixtures() else { return };
    let mut cm = ClassManager::new(&[], &[], &[]);
    let id = cm
        .define_class_with_options(
            "Foo",
            &v1,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("v1 define ok");
    assert!(!cm.is_class_bytes_pinned(id));
    assert_eq!(cached(&cm, id).as_deref(), Some(&v1[..]));
    flood_the_fifo(&mut cm);
    assert_eq!(
        cached(&cm, id),
        None,
        "an unpinned base must still be evictable"
    );
    assert_eq!(cm.class_bytes_pinned_size(), 0);
}

#[test]
fn a_load_time_transformed_base_survives_the_fifo() {
    // A non-retransformable transformer changed the class at load: the
    // transformed file is the base, and no class-path file equals it.
    let Some((v1, v2)) = fixtures() else { return };
    let mut cm = ClassManager::new(&[], &[], &[]);
    cm.stage_transformed_class("Foo", v2.clone(), ClassLoaderId::Application, None);
    let id = cm.load_class("Foo").expect("the staged class loads");
    assert!(
        cm.is_class_bytes_pinned(id),
        "a load-time transformed base is pinned"
    );
    flood_the_fifo(&mut cm);
    assert_eq!(
        cached(&cm, id).as_deref(),
        Some(&v2[..]),
        "the transformed file is the base and must survive the FIFO"
    );
    assert_eq!(cm.class_bytes_pinned_size(), v2.len());
    assert_eq!(cm.class_bytes_match_base(id, &v1), Some(false));
}

#[test]
fn a_retransform_capable_transformers_input_survives_the_fifo() {
    // A retransform-capable transformer changed the class at load: the base is
    // the file it was handed (`stage_transformed_class`'s fourth argument).
    let Some((v1, v2)) = fixtures() else { return };
    let mut cm = ClassManager::new(&[], &[], &[]);
    cm.stage_transformed_class(
        "Foo",
        v2.clone(),
        ClassLoaderId::Application,
        Some(v1.clone()),
    );
    let id = cm.load_class("Foo").expect("the staged class loads");
    flood_the_fifo(&mut cm);
    assert_eq!(cached(&cm, id).as_deref(), Some(&v1[..]));
    assert_eq!(
        cm.class_bytes_pinned_size(),
        v1.len(),
        "one pinned file, not two"
    );
}

#[test]
fn a_redefined_base_survives_the_fifo_and_a_retransform_keeps_it() {
    let Some((v1, v2)) = fixtures() else { return };
    let mut cm = ClassManager::new(&[], &[], &[]);
    let id = cm
        .define_class_with_options(
            "Foo",
            &v1,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("v1 define ok");
    cm.redefine_class(id, v2.clone(), RedefineOptions::default())
        .expect("redefine to v2 ok");
    assert!(
        cm.is_class_bytes_pinned(id),
        "a redefinition's file is pinned"
    );
    flood_the_fifo(&mut cm);
    assert_eq!(cached(&cm, id).as_deref(), Some(&v2[..]));

    // A retransformation installs other bytes and keeps the base, pinned.
    let options = RedefineOptions {
        preserve_original_bytes: true,
        ..RedefineOptions::default()
    };
    cm.redefine_class(id, v1.clone(), options)
        .expect("retransform back to v1 ok");
    flood_the_fifo(&mut cm);
    assert_eq!(cached(&cm, id).as_deref(), Some(&v2[..]));
    assert!(cm.is_class_bytes_pinned(id));
    assert_eq!(cm.class_bytes_pinned_size(), v2.len());
}

#[test]
fn the_pinned_bases_are_bounded() {
    // Past the pinned cap a base goes to the FIFO (evictable, as before), so a
    // run that transforms every class cannot grow the cache without bound.
    let Some((v1, v2)) = fixtures() else { return };
    let mut cm = ClassManager::new(&[], &[], &[]);
    cm.set_class_bytes_pinned_cap(v2.len() - 1);
    let id = cm
        .define_class_with_options(
            "Foo",
            &v1,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .expect("v1 define ok");
    cm.redefine_class(id, v2.clone(), RedefineOptions::default())
        .expect("redefine to v2 ok");
    assert!(!cm.is_class_bytes_pinned(id), "over the cap: not pinned");
    assert_eq!(cm.class_bytes_pinned_size(), 0);
    assert_eq!(cached(&cm, id).as_deref(), Some(&v2[..]));
    assert_eq!(cm.class_bytes_match_base(id, &v2), Some(true));
    flood_the_fifo(&mut cm);
    assert_eq!(cached(&cm, id), None);
}
