// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 20 wave 11, lane pAtom (W9L2-4): the twelve `Math.*Exact` rows
//! (`{add,subtract,multiply,increment,decrement,negate}Exact`, `int` and
//! `long`) are claimed LEAF (`NativeMethodRegistry::set_leaf`) unless
//! `CRATONVM_MATH_EXACT_LEAF=0`. This pins the claim for both `Math` and
//! `StrictMath`, keeps `floorDiv` / `toIntExact` off the list, and fails if a
//! body grows a call that would break the leaf contract (the overflow arm must
//! stay an `Err(MethodCallFailed)` built from a Rust message, never a Java
//! allocation or a JNI-pending exception).

use cratonvm_native_api::NativeMethodRegistry;

const ROWS: [(&str, &str); 12] = [
    ("addExact", "(II)I"),
    ("addExact", "(JJ)J"),
    ("subtractExact", "(II)I"),
    ("subtractExact", "(JJ)J"),
    ("multiplyExact", "(II)I"),
    ("multiplyExact", "(JJ)J"),
    ("incrementExact", "(I)I"),
    ("incrementExact", "(J)J"),
    ("decrementExact", "(I)I"),
    ("decrementExact", "(J)J"),
    ("negateExact", "(I)I"),
    ("negateExact", "(J)J"),
];

fn body<'a>(src: &'a str, head: &str) -> &'a str {
    let start = src.find(head).unwrap_or_else(|| panic!("{head} not found"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("end of fn");
    &rest[..end]
}

