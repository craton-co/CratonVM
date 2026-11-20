// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

#![deny(
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::undocumented_unsafe_blocks
)]

//! Java I/O native methods for CratonVM.
//!
//! Contains native method implementations for java.io and java.nio I/O classes.
//! The FileDescriptorTable is provided by cratonvm-native-api.
//!
//! # SECURITY
//!
//! This crate gives running Java code access to the host filesystem,
//! processes, and network. **It is NOT a sandbox by default.** Path
//! validation (see [`validate_path`]) rejects `..` traversal segments and
//! null bytes, but does **not** confine resolved paths to any directory —
//! absolute paths and symlink escapes are accepted.
//!
//! Embedders running **untrusted bytecode or hosting multiple tenants**
//! MUST, at startup, call [`set_path_confine_to_cwd`]`(true)` to enable
//! CWD confinement and register any extra trusted directories with
//! [`add_sandbox_root`]. SECURITY FIX (V12): alternatively, set the
//! `CRATONVM_CONFINE_IO` (certified, fail-closed) or `CRATONVM_UNTRUSTED_CODE`
//! (warning) environment variable at startup and the runtime auto-enables CWD
//! confinement for you — see [`validate_path`]. The zip/jar natives additionally cap the size of a
//! single inflated entry to guard against decompression bombs; see
//! `zip_real_jar::DEFAULT_MAX_ENTRY_BYTES` and the
//! `CRATONVM_ZIP_MAX_ENTRY_BYTES` environment variable.
//!
//! # Platform support
//!
//! For the per-feature Linux / Windows / macOS matrix (File I/O,
//! FileChannel.mmap / lock, AIO, Selector, Sockets, UDP, Multicast,
//! TLS, Pipe, WatchService, Process spawn) and the env vars / runtime
//! flags that affect platform behaviour, see
//! [`docs/PLATFORMS.md`](https://github.com/craton-co/CVM/blob/main/docs/PLATFORMS.md)
//! in the workspace root.

/// The I/O and networking slice of the process-wide typed configuration.
///
/// Every `CRATONVM_*` flag this crate reads is a field on
/// [`cratonvm_types::IoFlags`], parsed once at first use. This crate used to
/// carry its own `env_flag_enabled` boolean parser, one of the five
/// inconsistent truth tables catalogued in `flag-census.md`; the
/// parser now lives in `cratonvm_types::flags::parse::truthy_word` with its
/// semantics unchanged.
#[inline]
pub(crate) fn io_flags() -> &'static cratonvm_types::IoFlags {
    &cratonvm_types::flags().io
}

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use cratonvm_native_api::fd_table::FdId;
use cratonvm_native_api::{
    layout_alias, read_alias, NativeContext, NativeKind, NativeMethodRegistry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::ArrayElementType;
// The heap's own object-kind discriminant. `bb_resolve_heap_array` uses it to
// refuse a non-array where a backing array is required — see
// W7-83-segment-as-backing-array.md.
use cratonvm_types::ObjectKind;
use cratonvm_types::{ClassId, ObjectRef, Value};

// EINTR-transparent socket I/O — the shared retry primitive the blocking
// socket and TLS paths funnel through. See the module docs for why
// `SA_RESTART` does not cover the sockets CratonVM actually uses.
pub mod eintr;
// libnio's / libnet's failure-exception rules (`strerror` text, `map0` ENOMEM,
// `handleSocketError`), shared by every NIO native in this crate.
pub mod libnio_error;
pub mod nio_native;
pub mod random_access_file;
// T16.5: MulticastSocket overrides + shared helpers for async channels.
pub mod net;
// T19.7.a: Selector / SelectionKey / SelectableChannel NIO primitives.
pub mod nio_selector;
pub mod stream_decoder;
pub mod stream_encoder;
// RA.7: real-mode JarFile / ZipFile natives via the `zip` crate.
pub mod zip_real_jar;

// Wave 3 — NIO / async I/O.
// WP3.3 + WP3.6 — real FileChannel.map (memmap2) + transferTo (sendfile/TransmitFile).
pub mod file_channel;
// `FileChannelImpl.read/write(ByteBuffer)` collapsed into one native call —
// see `filechannel-heap-read-glue-depth-FIXED-20260823.md`.
pub mod file_channel_fast_read;
// WP3.4 — non-blocking SocketChannel / ServerSocketChannel with EAGAIN semantics.
pub mod socket_channel;
// Per-call cost removal shared by the socket transfer and selector paths: the
// reusable transfer buffer that replaces `vec![0u8; remaining]`, the
// per-`ClassId` `ByteBuffer` layout cache that replaces `get_field_by_name`,
// and the engagement census that says whether either engaged.
pub mod socket_fast_io;
// Real non-blocking TCP connect with a pollable OS fd (ES-HANG-02 residual 1).
pub mod nb_connect;
// AF_UNIX stream sockets backing `*.open(StandardProtocolFamily.UNIX)` — the
// `unixDomainSocketPath` connector shape (Tomcat `NioEndpoint`). Used by
// `socket_channel`.
pub mod uds;
// WP3.2 — AsynchronousSocketChannel / AsynchronousServerSocketChannel + AsynchronousChannelGroup.
pub mod async_socket;
// Task #16 — SSRF outbound-policy hook + configurable connect timeout shared
// by the async path (`async_socket`) and the blocking NIO path
// (`socket_channel`, `net`). Default policy blocks link-local cloud-metadata
// IPs; default timeout is 30 s.
pub mod outbound_policy;
// WP3.5 — DirectByteBuffer real allocation + Bits accounting + power-of-two pool.
pub mod direct_buffer;
// WP3.7 — Pipe.open() backed by libc::pipe / CreatePipe.
/// Minting a receiver of the class the real JDK would construct, instead of
/// the ABSTRACT public API class this crate's factories used to name — and
/// keeping the registration in step with the mint. `H21-1` N3.
pub mod concrete_receiver;

pub mod pipe;
// WP3.7 — DatagramChannel send/receive/multicast.
pub mod datagram;
// WP3.8 — WatchService backed by `notify` (inotify / ReadDirectoryChangesW / FSEvents).
pub mod watch;
// WP1.12 — ProcessBuilder / Process / ProcessHandleImpl subprocess bridge.
pub mod process;

#[cfg(test)]
mod test_support;

#[cfg(test)]
thread_local! {
    static PATH_CONFINE_TO_CWD_TEST_OVERRIDE: std::cell::Cell<Option<bool>> =
        std::cell::Cell::new(None);
}

/// Enable or disable confining canonicalized paths to the process CWD.
/// Off by default — see [`PATH_CONFINE_TO_CWD`].
#[cfg(not(test))]
pub fn set_path_confine_to_cwd(enabled: bool) {
    PATH_CONFINE_TO_CWD.store(enabled, Ordering::Relaxed);
}

#[cfg(test)]
pub fn set_path_confine_to_cwd(enabled: bool) {
    PATH_CONFINE_TO_CWD_TEST_OVERRIDE.with(|cell| cell.set(Some(enabled)));
}

#[cfg(test)]
mod zr8_fm_jimage_direct_map_tests;
/// gci i2/jm: `NativeImageBuffer.getNativeMap` answers a direct buffer over
/// one process-wide read-only map of the image (default), or the heap copy
/// (`CRATONVM_JIMAGE_DIRECT_MAP=0`). The arms are driven through
/// `jimage_native_map_for`, so the environment does not decide which one a
/// test exercises.
#[cfg(test)]
mod gci_i2jm_jimage_map_tests;
/// Round 15 wave 4 (lane walk3): `native_is_transfer_to` is JDK 25's loop.
#[cfg(test)]
mod r15w4_walk3_transfer_to_tests;
/// `slice`/`slice(int,int)`/`duplicate`/`asReadOnlyBuffer`/`order` are
/// `public abstract` on every typed NIO buffer subclass in real JDK 25
/// (CharBuffer/IntBuffer/LongBuffer/FloatBuffer/DoubleBuffer/ShortBuffer) —
/// unlike ByteBuffer, where all five are concrete bytecode. Every factory
/// above (`alloc_typed_buffer`) and the `ByteBuffer.asXxxBuffer()` views in
/// `native-builtins/src/servlet.rs` (`s2_view_buf_fn!`) stamp the returned
/// object with the LITERAL abstract class name (e.g. `java/nio/FloatBuffer`,
/// not a concrete `HeapFloatBuffer`/`ByteBufferAsFloatBufferB`), so an
/// `invokevirtual` for any of these five against that receiver resolves to a
/// Code-less abstract declaration and throws AbstractMethodError unless
/// registered directly here — same shape as the `get`/`put`/`compact`
/// registrations already in each loop below. See
/// ES-FAIL-FAMILY-20260710-floatbuffer-abstract-receiver-nocode-FIXED.md.
macro_rules! tb_abstract_view_fns {
    ($slice_fn:ident, $slice2_fn:ident, $dup_fn:ident, $ro_fn:ident, $order_fn:ident, $cls:literal, $elem:expr, $suffix:literal) => {
        /// `slice()` — F37-1 §2, landing F26-1 §9.1 for the typed families.
        ///
        /// **This COPIED, and a copy cannot express three properties HotSpot
        /// guarantees.** MEASURED, jdk-25.0.3+9, identical in all six families
        /// (`scratchpad/f37/F37TypedAliasProbe.java`, 691 rows mechanically
        /// diffed as structurally identical after normalising the family name
        /// and the element width):
        ///
        /// ```text
        /// <fam>.w.pos(2).slice class          java.nio.Heap<fam>Buffer
        /// <fam>.w.pos(2).slice geom           0/6/6
        /// <fam>.w.pos(2).slice array()==src   true    <- IDENTITY, not equality
        /// <fam>.w.pos(2).slice arrayOffset    2
        /// <fam> slice write seen by src       99      <- SHARED, both directions
        /// <fam> src write seen by slice       77
        /// <fam>.w.slice(3,4) arrayOffset      3       <- position IGNORED
        /// <fam>.w.slice().slice(1,2).arrayOffset  3   <- offsets COMPOSE
        /// <fam>.allocate(4).pos(4).slice().arrayOffset  4  <- EMPTY slice, offset still moves
        /// <fam>.direct.view.slice isDirect    true
        /// <fam>.direct.view.slice.get(0)      21      <- direct stays SHARED too
        /// ```
        ///
        /// `arrayOffset() == 2` is not merely wrong from a copy, it is
        /// UNREACHABLE from one: a fresh buffer's offset can only be 0. And the
        /// lost write is the quiet kind — a program that slices a buffer, hands
        /// the slice to a decoder and reads the result back through the
        /// original got the ORIGINAL elements, with no exception anywhere.
        ///
        /// The window arithmetic is [`slice_window`], REUSED UNCHANGED from the
        /// `ByteBuffer` repair: real `HeapIntBuffer.slice()` is
        /// `new HeapIntBuffer(hb, -1, 0, rem, rem, pos + offset, segment)` —
        /// the same five arguments as `HeapByteBuffer`'s, with `offset` counted
        /// in ELEMENTS instead of bytes. Only the `address` seed needs the
        /// element width ([`typed_buffer_address`]).
        ///
        /// Which receivers alias and which still copy is [`tb_alias_source`],
        /// and the copy fallback below is byte-for-byte the behaviour that was
        /// here before — plus the pin [`tb_alloc_copy_target`] adds.
        pub(crate) fn $slice_fn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // MIGRATED — receiver is caller-supplied and may be a direct view
            // (`ByteBuffer.allocateDirect(n).asIntBuffer()` is direct on
            // HotSpot). `new_buf` is the freshly minted heap buffer, so its
            // own view is structurally heap; it still goes through the same
            // element accessor so the two halves cannot drift apart.
            let view = bb_state(ctx, this)?;
            let src_offset = match view.storage {
                BbStorage::Heap { offset, .. } => offset,
                BbStorage::Direct { .. } => 0,
            };
            let window = slice_window(src_offset, view.pos, view.lim);
            // Read the flag BEFORE deriving: everything below allocates, and
            // `this` must not be dereferenced afterwards. This is the hazard
            // `buf_write_read_only` exists for, and the old body had it — it
            // called `buf_stamp_read_only(ctx, this, …)` AFTER
            // `alloc_typed_buffer`, i.e. it read a field off a reference that
            // two allocations could have invalidated.
            let src_ro = buffer_is_read_only(ctx, this);
            if let Some(aliased) = tb_derive_view(ctx, this, view, window, $cls, $elem) {
                buf_write_read_only(ctx, aliased, BufferDerivation::Inherit, src_ro);
                return Ok(Some(Value::Object(Some(aliased))));
            }
            let remaining = window.cap.max(0) as usize;
            let rel_start = window.offset.saturating_sub(src_offset);
            let (new_buf, view) = tb_alloc_copy_target(ctx, view, $cls, $elem, remaining);
            let new_view = bb_state(ctx, new_buf)?;
            for i in 0..remaining {
                let v = tb_read_elem(ctx, view, rel_start + i)?;
                tb_write_elem(ctx, new_view, i, v)?;
            }
            buf_write_metadata(
                ctx,
                new_buf,
                window.pos,
                window.lim,
                window.cap,
                window.mark,
            );
            // Read-only is CONTAGIOUS — see [`BufferDerivation`]. All three of
            // `$slice_fn`/`$slice2_fn`/`$dup_fn` wrote nothing to `isReadOnly`,
            // so every one of the six typed families handed a WRITABLE view
            // back from a read-only source.
            buf_write_read_only(ctx, new_buf, BufferDerivation::Inherit, src_ro);
            Ok(Some(Value::Object(Some(new_buf))))
        }

        pub(crate) fn $slice2_fn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let index = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let length = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            // MIGRATED — same reasoning as `$slice_fn` above.
            let view = bb_state(ctx, this)?;
            // The bounds check runs BEFORE anything is allocated, and against
            // `limit()` — `Objects.checkFromIndexSize(index, length, limit())`
            // in the JDK body. Unchanged; only the storage decision below is.
            buffer_check_from_index_size(index, length, view.cap)?;
            let src_offset = match view.storage {
                BbStorage::Heap { offset, .. } => offset,
                BbStorage::Direct { .. } => 0,
            };
            // ABSOLUTE-indexed: `index` is a limit-space index and the source's
            // `position` plays no part. MEASURED:
            // `<fam>.allocate(8).position(2).slice(3,4)` has `arrayOffset()==3`,
            // not 5 — so `position` really is ignored, and a window computed
            // from `pos + index` would be wrong by the position on every call.
            let window = slice_range_window(src_offset, index, length);
            let src_ro = buffer_is_read_only(ctx, this);
            if let Some(aliased) = tb_derive_view(ctx, this, view, window, $cls, $elem) {
                buf_write_read_only(ctx, aliased, BufferDerivation::Inherit, src_ro);
                return Ok(Some(Value::Object(Some(aliased))));
            }
            let count = window.cap.max(0) as usize;
            let rel_start = window.offset.saturating_sub(src_offset);
            let (new_buf, view) = tb_alloc_copy_target(ctx, view, $cls, $elem, count);
            let new_view = bb_state(ctx, new_buf)?;
            for i in 0..count {
                let v = tb_read_elem(ctx, view, rel_start + i)?;
                tb_write_elem(ctx, new_view, i, v)?;
            }
            buf_write_metadata(
                ctx,
                new_buf,
                window.pos,
                window.lim,
                window.cap,
                window.mark,
            );
            buf_write_read_only(ctx, new_buf, BufferDerivation::Inherit, src_ro);
            Ok(Some(Value::Object(Some(new_buf))))
        }

        pub(crate) fn $dup_fn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // MIGRATED — receiver is caller-supplied and may be direct.
            let view = bb_state(ctx, this)?;
            let mark = buf_read_mark(ctx, this);
            let src_offset = match view.storage {
                BbStorage::Heap { offset, .. } => offset,
                BbStorage::Direct { .. } => 0,
            };
            // `duplicate()` carries the source's `offset` ACROSS UNCHANGED, and
            // that is the half F21 left out of the ByteBuffer twin and F26-1 §3
            // then found live on dev: a duplicate that shares the array but
            // resets `offset` to a fresh allocation's 0 reads from the wrong
            // element with no exception. MEASURED here for the typed families:
            // `<fam>.w.slice().duplicate().arrayOffset()` is `2`, and
            // `<fam>.w.duplicate()` keeps `2/8/8` where `slice()` gives
            // `0/6/6`. `duplicate` also KEEPS the mark where `slice` drops it.
            let window = duplicate_window(src_offset, view.pos, view.lim, view.cap, mark);
            let src_ro = buffer_is_read_only(ctx, this);
            if let Some(aliased) = tb_derive_view(ctx, this, view, window, $cls, $elem) {
                buf_write_read_only(ctx, aliased, BufferDerivation::Inherit, src_ro);
                return Ok(Some(Value::Object(Some(aliased))));
            }
            let cap_usize = window.cap.max(0) as usize;
            let (new_buf, view) = tb_alloc_copy_target(ctx, view, $cls, $elem, cap_usize);
            let new_view = bb_state(ctx, new_buf)?;
            for i in 0..cap_usize {
                let v = tb_read_elem(ctx, view, i)?;
                tb_write_elem(ctx, new_view, i, v)?;
            }
            buf_write_metadata(
                ctx,
                new_buf,
                window.pos,
                window.lim,
                window.cap,
                window.mark,
            );
            buf_write_read_only(ctx, new_buf, BufferDerivation::Inherit, src_ro);
            Ok(Some(Value::Object(Some(new_buf))))
        }

        /// `asReadOnlyBuffer()` — `$dup_fn` plus the ONE-WAY stamp.
        ///
        /// The `Int(1)` this used to write by hand was right, and stayed right
        /// once `$dup_fn` began propagating: `ForceReadOnly` is unconditional,
        /// so it overrides `$dup_fn`'s `Inherit` in the writable-source case
        /// and agrees with it in the read-only-source case. Routed through
        /// [`buffer_view_read_only`] anyway so the one-way-ness is a decision
        /// the shared function makes and a test can pin, rather than a literal
        /// at six expansion sites. MEASURED: no JDK operation clears the flag —
        /// `ro.duplicate().asReadOnlyBuffer().isReadOnly()` is `true` and there
        /// is no `asWritableBuffer`.
        pub(crate) fn $ro_fn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            let src = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return $dup_fn(ctx, args),
            };
            let result = $dup_fn(ctx, args)?;
            if let Some(Value::Object(Some(o))) = result {
                buf_stamp_read_only(ctx, src, o, BufferDerivation::ForceReadOnly);
            }
            Ok(result)
        }

        pub(crate) fn $order_fn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            let order_name = match args.first() {
                Some(Value::Object(Some(this))) => {
                    let cname = ctx
                        .class_name_of_id(ctx.class_id_of_object(*this))
                        .unwrap_or_default();
                    if cname.ends_with(concat!($suffix, "B"))
                        || cname.ends_with(concat!($suffix, "RB"))
                    {
                        "BIG_ENDIAN"
                    } else if cname.ends_with(concat!($suffix, "L"))
                        || cname.ends_with(concat!($suffix, "RL"))
                    {
                        "LITTLE_ENDIAN"
                    } else {
                        "NATIVE_ORDER"
                    }
                }
                _ => "NATIVE_ORDER",
            };
            Ok(Some(Value::Object(Some(tb_static_object(
                ctx,
                "java/nio/ByteOrder",
                order_name,
            )?))))
        }
    };
}

// Windows: use LockFileEx / UnlockFileEx via raw FFI — same pattern as
// `pipe.rs`'s `CreatePipe` declaration, avoids pulling in `windows-sys`.
#[cfg(target_family = "windows")]
mod win_lock {
    use std::ffi::c_void;

    pub(super) type Handle = *mut c_void;
    pub(super) type Bool = i32;
    pub(super) type Dword = u32;

    pub(super) const LOCKFILE_EXCLUSIVE_LOCK: Dword = 0x0000_0002;
    pub(super) const LOCKFILE_FAIL_IMMEDIATELY: Dword = 0x0000_0001;

    #[repr(C)]
    pub(super) struct Overlapped {
        pub internal: usize,
        pub internal_high: usize,
        pub offset: Dword,
        pub offset_high: Dword,
        pub h_event: Handle,
    }

    #[link(name = "Kernel32")]
    extern "system" {
        pub(super) fn LockFileEx(
            h_file: Handle,
            dw_flags: Dword,
            dw_reserved: Dword,
            n_bytes_low: Dword,
            n_bytes_high: Dword,
            lp_overlapped: *mut Overlapped,
        ) -> Bool;
        pub(super) fn UnlockFileEx(
            h_file: Handle,
            dw_reserved: Dword,
            n_bytes_low: Dword,
            n_bytes_high: Dword,
            lp_overlapped: *mut Overlapped,
        ) -> Bool;
    }
}

macro_rules! afc_trace {
    ($($arg:tt)*) => {
        if afc_trace_enabled() {
            // Millisecond stamp: the one question this family keeps asking is
            // "how long did that take", and a bare line cannot answer it. The
            // 2026-08-16 asynchronous-submit experiment turned on exactly this
            // reading -- `close` entered and `close ... cancelled-pending-io`
            // 58.5 s later.
            eprintln!(
                "[dbg-afc {}] {}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() % 1_000_000)
                    .unwrap_or(0),
                format!($($arg)*)
            );
        }
    };
}

#[cfg(test)]
mod gcg_g3rw_orphan_fd_tests;
// ---------------------------------------------------------------------------
// 92.2: WatchService (File System Events)  —  T2.4.13
//
// Real platform-native implementation backed by the `notify` crate, which
// dispatches to `inotify` on Linux, `FSEvents`/`kqueue` on macOS, and
// `ReadDirectoryChangesW` on Windows. Each `WatchService` owns one
// `RecommendedWatcher` + a bounded channel. Events drained from the
// channel are partitioned per registered path and handed to the Java
// side through the existing WatchKey / WatchEvent synthetic layout.
// ---------------------------------------------------------------------------

use notify::{
    event::{CreateKind, EventKind, ModifyKind, RemoveKind},
    RecursiveMode, Watcher as NotifyWatcher,
};
use std::sync::mpsc;
/// `return` the receiver's bytecode result when [`watch_yield_if_real`] says so.
macro_rules! watch_real_yield {
    ($ctx:expr, $args:expr, $w:expr, $m:literal, $d:literal) => {
        if let Some(r) = watch_yield_if_real($ctx, $args, $w, $m, $d) {
            return r;
        }
    };
}

// ---------------------------------------------------------------------------
// 92.4: Real Selector — DELETED (W7-66, 2026-08-12)
// ---------------------------------------------------------------------------
//
// `register_selector` and the ten natives only it installed
// (`native_sel_open`, `native_channel_register`, `do_select`,
// `native_sel_select{,_timeout,_now}`, `native_sel_selected_keys`,
// `native_sel_keys`, `native_sel_wakeup`, `native_sel_close`,
// `native_sel_is_open`) are gone. The registrar lost its only caller in
// Wave 3 / Task C — see the note in `register_nio_channel_extras` — and
// `nio_selector.rs::register_nio_selector`, called from `register_io_natives`,
// has been the source of truth for `java/nio/channels/Selector` and
// `SelectionKey` since. Deadness confirmed by grep before deletion: the
// registrar's only reference in the workspace was its own definition, and
// every native it installed had exactly one other reference, its own `fn`.
//
// Five of W7-59's twenty-eight `over` census rows lived here — `SelectionKey`
// 4-vs-1 and `HashSet` 2-vs-1 — and were noise in every future run of
// `CRATONVM_DBG_LAYOUT_ALIAS`. They are the "5 dead" column, now zero.
//
// The `SelectionKey.OP_READ`/`OP_WRITE`/`OP_CONNECT`/`OP_ACCEPT` accessors
// this registrar carried under a "KEEP" comment went with it, and nothing
// changes: an unreachable registrar never installed them. `nio_selector.rs`
// declares the same four constants (`pub const OP_READ: i32 = 1`, ...).

// ===========================================================================
// Comprehensive I/O tests
// ===========================================================================

#[cfg(test)]
mod io_tests;

// ===========================================================================
// T2.4.18/19 — Modified UTF-8 encoder / decoder unit tests
// ===========================================================================
#[cfg(test)]
mod t2_mutf8_tests;

#[cfg(test)]
mod ra2_utf8_decoder_tests;

// ===========================================================================
// RA.3 — `Reader.read(CharBuffer)` native, with a mock NativeContext.
// ===========================================================================
#[cfg(test)]
mod ra3_reader_read_charbuffer_tests;

// ===========================================================================
// BAIS layout — real-JDK slot indices { buf=0, pos=1, mark=2, count=3 }
// ===========================================================================
#[cfg(test)]
mod bais_layout_tests;

// ===========================================================================
// Bounds-check coverage for the ByteBuffer / ByteArrayInputStream bulk and
// absolute natives (fable-2026-06-10 review B1/B2/B3): negative / overflow /
// past-the-end indices must raise an exception, not silently drop the write.
// ===========================================================================
#[cfg(test)]
mod buffer_bounds_tests;

#[cfg(test)]
mod files_bulk_transfer_tests;

// ===========================================================================
// Regression: File.getAbsolutePath must NOT canonicalize (no symlink
// resolution, no Windows `\\?\` verbatim prefix). That behaviour belongs to
// getCanonicalPath. An already-absolute path is returned verbatim.
// ===========================================================================
#[cfg(test)]
mod abs_path_tests;

// ===========================================================================
// java.nio `Buffer.address` — the indexed-slot aliasing guard
// ===========================================================================

/// `BB_FIELD_MARK` is index 4, and index 4 on a real-JDK `java.nio.Buffer` is
/// `address`, not `mark`. These tests pin the two halves of the fix: mutators
/// must not disturb an address the object already carries, and every allocator
/// that hands back a heap-backed buffer must give it one.
#[cfg(test)]
mod nio_buffer_address_tests;

/// Round 16 wave 16 (lane nio2): the `MappedByteBuffer` receiver screen
/// (NIO-4) and the `--real-jdk` retirement of the three rows.
#[cfg(test)]
mod r16w16_nio2_mapped_buffer_tests;
/// JIT round 23 wave 1, lane L6 (NIO20W3-L5-3 / -4): VM-opened descriptors
/// carry their id in the platform's field, and every stream reader goes
/// through `nio_native::descriptor_fd_id`.
#[cfg(test)]
mod r23_w1_l6_descriptor_fields_tests;

mod lib_parts;
#[allow(unused_imports)]
pub use lib_parts::native_noop::*;
#[allow(unused_imports)]
use lib_parts::native_noop::*;
#[allow(unused_imports)]
pub use lib_parts::native_bais_read_byte_array::*;
#[allow(unused_imports)]
use lib_parts::native_bais_read_byte_array::*;
#[allow(unused_imports)]
pub use lib_parts::register_io_natives::*;
#[allow(unused_imports)]
use lib_parts::register_io_natives::*;
#[allow(unused_imports)]
pub use lib_parts::register_nio_natives::*;
#[allow(unused_imports)]
use lib_parts::register_nio_natives::*;
#[allow(unused_imports)]
pub use lib_parts::native_sr_skip::*;
#[allow(unused_imports)]
use lib_parts::native_sr_skip::*;
#[allow(unused_imports)]
pub use lib_parts::native_files_create_directories::*;
#[allow(unused_imports)]
use lib_parts::native_files_create_directories::*;
#[allow(unused_imports)]
pub use lib_parts::os_acquire_lock::*;
#[allow(unused_imports)]
use lib_parts::os_acquire_lock::*;
#[allow(unused_imports)]
pub use lib_parts::wrap_completed_future::*;
#[allow(unused_imports)]
use lib_parts::wrap_completed_future::*;
