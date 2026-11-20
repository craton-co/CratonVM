// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! NIO, HTTP client, and resource loading natives.

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
// The ONE copy of the `hasArray`/`array`/`arrayOffset` three-way rule and of
// the two one-liners that feed it. `cratonvm-native-builtins` depends on
// `cratonvm-native-io` (`Cargo.toml`) and not the reverse, so `native-io` is
// the only crate whose copy this file and `phases_late/charset_buffers.rs` can
// both import — which is why F14-1 made them `pub` there rather than moving
// them here. Record: F14-1 N1, landed by F21-1.
use cratonvm_native_io::{buffer_array_access, BufferArrayAccess};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
// The heap's own object-kind discriminant. `s2_bb_arr` uses it to refuse a
// `java.nio.Buffer.segment` that is a `MemorySegment` rather than a backing
// array — see W7-83-segment-as-backing-array.md.
use cratonvm_types::ObjectKind;
use cratonvm_types::{ObjectRef, Value};

use crate::phases_late::{
    p56_build_stream, p58_new_cf, CLEANABLE_ACTION, CLEANABLE_CLEANED, CLEANABLE_FIELDS,
    CLEANABLE_INDEX, REF_TYPE_CLEANER,
};
use crate::{native_noop_with_this, obj_arg, try_alloc_concurrent_synthetic};

use std::collections::HashMap;
use std::io::{Read as StdRead, Write as StdWrite};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, OnceLock};
/// [`order_chain_from_leaf`], against the input the live platform declines to
/// produce.
///
/// MEASURED: with the walk skipped, SChannel returned leaf-first on all six
/// hosts of `PeerChainOrderProbe` — so a probe against the real world cannot
/// tell the walk from its absence, and the only honest way to exercise it is
/// to hand it the shuffled set the store is permitted to hand back but
/// currently does not.
///
/// The certificates are two-byte stand-ins and the `names` closure is a lookup
/// table, deliberately: what is under test is the subject/issuer walk, not the
/// DER parser, and a fixture built from real certificates would test both at
/// once and be unbuildable on the platform this code runs on (no `openssl`
/// crate on Windows).
#[cfg(test)]
mod chain_order_tests;
// ---- ByteBuffer ------------------------------------------------------------

#[allow(clippy::too_many_lines)]
// Named helpers for view-buffer creation (closures can't capture; need fn ptrs)
macro_rules! s2_view_buf_fn {
    ($name:ident, $cls:literal, $elem_sz:expr) => {
        fn $name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            let lim = s2_bb_limit(ctx, this);
            let rem = (lim - pos) / $elem_sz;
            let vb = try_alloc_concurrent_synthetic(ctx, $cls, 6)?;
            // BUG (found 2026-07-11): `ctx.get_field(this, BB_ARRAY)` reads
            // the SOURCE ByteBuffer's raw indexed slot 0 — but in real-JDK
            // mode `this` is allocated with the real Buffer+ByteBuffer field
            // layout (see `bb_write_hb`'s doc comment), where slot 0 is the
            // real `mark` field (an Int, almost always -1), NOT the backing
            // array. The actual array lives in the by-name `hb` field.
            // Reading the wrong slot silently propagated `Value::Int(-1)`
            // into the view's BB_ARRAY slot; every subsequent read on the
            // view then found no `Object` there, fell through `s2_bb_arr`'s
            // "no array" branch, and returned a benign 0 — every ES vector
            // read via `ByteBuffer.asFloatBuffer()`/`asIntBuffer()`/etc.
            // (i.e. essentially all quantized/HNSW/DiskBBQ vector value
            // reads) silently came back as an all-zero vector, surfacing as
            // "expected:<X> but was:<0.0>" across nearly the whole ES
            // vector-codec test family. `s2_bb_arr` resolves the by-name
            // `hb` field first (falling back to the indexed slot only for
            // genuinely storage-less synthetics), matching what
            // `s2_bb_as_char_buffer` already does correctly below.
            //
            // (The typed-buffer-view `slice`/`slice(II)`/`duplicate` natives
            // added alongside this fix's sibling commit do NOT have this bug
            // despite looking similar: their receiver is always an
            // abstract-stamped typed-buffer view, which — unlike ByteBuffer —
            // declares no `hb` field of its own, so `s2_bb_arr`'s by-name
            // lookup already falls straight through to the same indexed
            // slot 0 that a raw `ctx.get_field` reads.)
            if let Some(arr) = s2_bb_arr(ctx, this) {
                ctx.set_field(vb, BB_SEGMENT_SLOT, Value::Object(Some(arr)));
                // Byte-start marker: the view's element 0 lives at source
                // byte `pos` — PLUS the source's own array-base offset
                // (non-zero when the source is an aliasing `slice()`),
                // since the view's byte accessors resolve against the
                // shared array with the VIEW's own (absent → 0) offset.
                let bs = s2_bb_heap_base(ctx, this) as i32 + pos;
                ctx.set_field(vb, BB_MARK, Value::Int(-(bs + 1)));
            } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
                // DIRECT source (residual-doc item 3): keep a real native
                // address — folded to the source's current position — in
                // the view's `address` slot. The byte accessors reach it
                // via `s2_bb_direct_addr`, and `s2_typed_view_byte_start`
                // reads a positive address slot as byte-start 0.
                ctx.set_field_by_name(vb, "address", Value::Long(addr.saturating_add(pos as i64)));
            }
            ctx.set_field(vb, BB_POS, Value::Int(0));
            ctx.set_field(vb, BB_LIMIT, Value::Int(rem));
            ctx.set_field(vb, BB_CAP, Value::Int(rem));
            // Propagate the source buffer's order via the layout-aware
            // accessors (the source may be real-layout: order in `bigEndian`).
            let ord = s2_bb_order(ctx, this);
            s2_bb_set_order(ctx, vb, ord);
            // …and the source's read-only-ness, which is F21-1 N3's other
            // half and had never been propagated. `ByteBuffer.asIntBuffer()`
            // &c. are registered HERE AND NOWHERE ELSE (`native-io` registers
            // no `as<T>Buffer` descriptor), so unlike `$slice`/`$dup` in the
            // macro below this arm is the LIVE one in every mode.
            //
            // MEASURED, jdk-25.0.3+9 (`scratchpad/f37/F37TypedAliasProbe.java`):
            // `ByteBuffer.allocate(64).asReadOnlyBuffer().asIntBuffer()` is a
            // `java.nio.ByteBufferAsIntBufferRB` with `isReadOnly() == true`,
            // its `array()` raises `UnsupportedOperationException` (array-less
            // is asked FIRST — it is not a `ReadOnlyBufferException` row), and
            // its `.slice()` is read-only too. Identical for Long/Short/Float/
            // Double/Char and for the direct twin `DirectIntBufferRS`.
            //
            // Without this the typed views' new `$put` guard could be walked
            // straight around: take a read-only ByteBuffer, ask it for an
            // `asIntBuffer()` view, and write through the view into the
            // read-only buffer's own backing array.
            let src_read_only = s2_bb_is_read_only(ctx, this);
            s2_bb_set_read_only(ctx, vb, src_read_only);
            Ok(Some(Value::Object(Some(vb))))
        }
    };
}
s2_view_buf_fn!(s2_bb_as_int_buffer, "java/nio/IntBuffer", 4);
// S4 servlet stubs removed — a JVM does not implement servlet APIs natively.
// Web frameworks (Spring Boot, Tomcat, Jetty) work when the VM can execute
// their bytecode from the real .class files.

/// The client-connector tests that the whole existing TLS corpus could not
/// express.
///
/// WHY THIS MODULE EXISTS AT ALL, in the words of the defect it guards: every
/// TLS fixture in this tree uses a SELF-SIGNED certificate -- H2's baked
/// identity, the netty test certs, the regression-suite keystores. For a
/// self-signed peer the leaf IS the trust anchor, so a one-element chain
/// validates perfectly and "the client captured only the leaf" is invisible to
/// the entire corpus BY CONSTRUCTION. It took 20 live public sites to see it.
///
/// So the fixture here is deliberately the one shape none of those have: a CA
/// and a leaf SIGNED BY IT, presented as a two-certificate chain. That is the
/// generalisable part -- a self-signed fixture cannot exercise chain building,
/// and no number of them adds up to one that can.
#[cfg(all(test, unix))]
mod openssl_client_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod w20e_servlet_tests;

/// gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 29): the per-selector wakeup sockets.
#[cfg(test)]
mod w29d_selector_wakeup_tests;

mod keep_across;
#[allow(unused_imports)]
pub use keep_across::*;
#[allow(unused_imports)]
use keep_across::*;
mod s2_bb_arr;
#[allow(unused_imports)]
pub use s2_bb_arr::*;
#[allow(unused_imports)]
use s2_bb_arr::*;
mod register_s2_byteorder;
#[allow(unused_imports)]
pub use register_s2_byteorder::*;
#[allow(unused_imports)]
use register_s2_byteorder::*;
