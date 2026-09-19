// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Executable memory: the one type in this crate that owns a W^X mapping, from
//! the `alloc_executable` that creates it to the `free_executable` that returns
//! it.
//!
//! Split out of `lib.rs` on 2026-09-16, following `jfr_compile_decision`,
//! `osr_entry`, `inline_cache_pic` and `ea_ir_bridge`.
//!
//! # Why this is a real boundary and not just a section
//!
//! [`ExecutableBuffer`] is the crate's only handle on memory that the CPU will
//! fetch instructions from, and every rule about that memory is enforced by a
//! method on this one type and by nothing else:
//!
//!   * the mapping starts read-write and only ever becomes read-execute through
//!     [`ExecutableBuffer::finalize`], so a jump into a half-written body faults
//!     on instruction fetch rather than executing rubble;
//!   * every write after that flip is bracketed by a `platform::JitWriteScope`,
//!     which is what keeps Apple silicon's per-thread W^X toggle honest — the
//!     emit and patch methods are the only places that scope is entered for a
//!     code buffer, so "did this write take the toggle?" is answerable by
//!     reading this file alone;
//!   * the address range is registered with the code-region table on creation
//!     and deregistered in `drop`, which is what makes the `usize -> fn` casts
//!     elsewhere in the crate checkable rather than hopeful;
//!   * overflow and unencodable-codegen discards are two DIFFERENT states
//!     (`overflowed` vs `codegen_failed`) because one is worth retrying at a
//!     measured size and the other never is, and only this type can tell them
//!     apart.
//!
//! Every one of those is an invariant stated about a private field and checked
//! by a method of the same struct. The fields stayed private across this split:
//! nothing outside this file can reach the raw pointer, the length or the
//! published flag, which is what lets the list above be read as a proof rather
//! than as a convention.
//!
//! # Why `Drop` travelled with it
//!
//! The destructor is the only code in the crate that may unmap a code page, so
//! it belongs to the type that owns the page even though most of its body is
//! bookkeeping. Keeping it here is what allowed the fields to stay private —
//! had it stayed behind in `lib.rs` it would have needed `ptr`, `len`,
//! `capacity` and `published` widened to `pub(crate)`, and the encapsulation
//! argued for above would have been traded away for a file boundary.
//!
//! It brought exactly the two helpers nothing else calls
//! (`CRATONVM_DBG_JIT_UNMAP` and `CRATONVM_JIT_NEVER_FREE_CODE`) and nothing
//! more. What it did NOT bring is the reclamation protocol itself: the
//! authorised-reclaim gate, the code-free ring, the cap, the retirement queue
//! and their census all stay at the crate root, because `AuthorisedReclaim` is
//! entered from dozens of sites that have nothing to do with this buffer. The
//! destructor CALLS into that protocol (`reclaim_is_authorised`,
//! `record_code_free`, the `RECLAMATION` ledger) through the imports below; it
//! does not own it. That is the seam: this file owns the mapping, `lib.rs` owns
//! the question of when releasing one is safe.
//!
//! Glob-re-exported from the crate root, so every path a caller used before the
//! split still resolves -- the code moved, the API did not.

use crate::{
    dbg_jit_code_free_enabled, defer_orphaned_mapping, jit_code_regions, platform,
    reclaim_is_authorised, record_code_free, CompileError, ACTIVE_JIT_EXECUTIONS,
    CODE_FREE_AUTHORISED, CODE_FREE_PUBLISHED, COMMITTED_JIT_CODE_BYTES, PUBLISHED_CODE_FREES,
    RECLAMATION, UNQUEUED_PUBLISHED_CODE_FREES,
};

/// A buffer of executable machine code allocated via OS-level APIs.
///
/// Every platform enforces W^X (see `platform.rs`): the buffer is mapped
/// read-write, and [`finalize`](ExecutableBuffer::finalize) flips it to
/// read-execute (`VirtualProtect` to `PAGE_EXECUTE_READ` on Windows, `mprotect`
/// to `PROT_READ | PROT_EXEC` on Linux/FreeBSD and macOS). Until then, a jump
/// into it faults on instruction fetch.
pub struct ExecutableBuffer {
    pub(crate) ptr: *mut u8,
    pub(crate) len: usize,
    pub(crate) capacity: usize,
    /// Named codegen invariant that discarded this method when the discard was
    /// NOT a capacity problem — see
    /// [`mark_codegen_unencodable`](Self::mark_codegen_unencodable). `None`
    /// alongside a set `overflowed` means the buffer really was too small.
    pub(crate) codegen_failed: Option<&'static str>,
    /// Set when an `emit`/`emit_byte` call could not fit in the buffer.
    /// `estimated_size` in the x64 backend is a heuristic, so a pathological
    /// method can exceed it. Rather than panicking the whole process, the
    /// emit hot path records overflow here and the compile driver bails to
    /// the interpreter (returns `None`) after codegen.
    pub(crate) overflowed: bool,
    /// Total bytes codegen ASKED to emit, counted whether or not the write
    /// fit. `len` freezes at the first overflow, so it cannot answer "how big
    /// should this buffer have been?" — and without that number an overflow
    /// bail is indistinguishable from a method the JIT declined for any other
    /// reason. See [`wanted`](Self::wanted).
    pub(crate) wanted: usize,
    /// AUDIT: set once the owning `CompiledMethod` has been published into a
    /// dispatch surface, i.e. once a raw pointer into this buffer can be held
    /// by generated code. A free of a buffer with this bit clear is harmless
    /// whatever `ACTIVE_JIT_EXECUTIONS` reads; a free of one with it set is
    /// only safe if the retirement queue authorised it.
    pub(crate) published: bool,
    /// Which sizing heuristic allocated this buffer, for the overflow warning.
    ///
    /// Four independent estimates allocate executable buffers, and the warning
    /// named none of them — so an overflow flood was attributed by arithmetic
    /// on the printed `len`, and got attributed to the WRONG one
    /// (`basicerrorcontroller-jit-only-failure-20260731-FIXED.md`
    /// blamed the single-pass backend's estimate for a flood that was entirely
    /// the optimizing tier's).
    pub(crate) tag: &'static str,
}

// SAFETY: an `ExecutableBuffer` uniquely owns its mapping, like a `Vec<u8>`, so
// moving it to another thread moves that ownership and nothing else.
unsafe impl Send for ExecutableBuffer {}
// SAFETY: every write goes through `&mut self`. Once finalized and shared behind
// the JIT cache's `Arc`, a buffer is executed and read; the `&self` operations
// only read the mapping or change its protection through the OS.
unsafe impl Sync for ExecutableBuffer {}

impl ExecutableBuffer {
    /// Allocate a new executable buffer with the given capacity.
    pub fn new(capacity: usize) -> Option<Self> {
        let ptr = platform::alloc_executable(capacity)?;
        // Account the committed executable memory. `COMMITTED_JIT_CODE_BYTES`
        // tracks currently-mapped code and is the quantity the code-cache cap
        // bounds; the `try_compile` gate reads it to decide whether to keep
        // compiling. Bumped here (not in `Drop`, which fires only on the rare
        // free path) so the figure reflects live mappings.
        COMMITTED_JIT_CODE_BYTES.fetch_add(capacity, std::sync::atomic::Ordering::Relaxed);
        // Register this region for code pointer validation.
        // Poison is recovered, as everywhere else this lock is taken. Skipping
        // the registration after an unrelated panic would make every
        // `validate_code_ptr` into this buffer fail.
        jit_code_regions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .register(ptr, capacity);
        Some(Self {
            ptr,
            len: 0,
            capacity,
            overflowed: false,
            codegen_failed: None,
            wanted: 0,
            published: false,
            tag: "untagged",
        })
    }

    /// AUDIT: mark this buffer as reachable by generated code.
    ///
    /// Also the code-cache install count: a published body is counted once
    /// here and once more when its mapping is released (`Drop`), so installed
    /// minus reclaimed is exactly the published code still mapped. See
    /// [`jit_code_reclamation_stats`].
    #[inline]
    pub fn mark_published(&mut self) {
        if !self.published {
            use std::sync::atomic::Ordering::Relaxed;
            RECLAMATION.installed_bodies.fetch_add(1, Relaxed);
            RECLAMATION
                .installed_bytes
                .fetch_add(self.capacity as u64, Relaxed);
            RECLAMATION
                .installed_code_bytes
                .fetch_add(self.len as u64, Relaxed);
        }
        self.published = true;
    }

    /// Name the sizing heuristic that allocated this buffer. Shown by the
    /// overflow warnings, which otherwise cannot say which estimate was short.
    #[inline]
    pub fn set_tag(&mut self, tag: &'static str) {
        self.tag = tag;
    }

    /// Write bytes into the buffer at the current position.
    ///
    /// If the write would exceed capacity the buffer is marked
    /// [`overflowed`](Self::overflowed) and the write is skipped instead of
    /// panicking. `len` is never advanced past `capacity`, so the buffer
    /// stays safe to slice/patch; the compile driver is expected to check
    /// `overflowed()` and discard the result.
    ///
    /// The `overflowed` flag is sticky: once any emit has overflowed, every
    /// subsequent `emit`/`emit_byte` becomes a no-op. This freezes `len` at
    /// the point of first overflow so a later (smaller) emit cannot slip
    /// bytes in past the gap left by the dropped instruction and produce a
    /// silently misaligned code stream.
    pub fn emit(&mut self, bytes: &[u8]) {
        self.wanted += bytes.len();
        if self.overflowed || self.len + bytes.len() > self.capacity {
            self.overflowed = true;
            return;
        }
        // Per-thread write permission for this copy only: a no-op except on
        // macOS/ARM64, where `MAP_JIT` code pages are writable only inside a
        // scope. See `platform::JitWriteScope`.
        let _write = platform::JitWriteScope::enter();
        // SAFETY: `len + bytes.len() <= capacity` was checked above, `ptr` is this buffer's own writable mapping of `capacity` bytes, and `&mut self` rules out `bytes` aliasing it.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(self.len), bytes.len());
        }
        self.len += bytes.len();
    }

    /// Write bytes into the buffer, returning `false` (without writing) if
    /// there is not enough capacity instead of panicking.
    ///
    /// Honours the same sticky [`overflowed`](Self::overflowed) state as
    /// [`emit`](Self::emit): once any earlier write was dropped, this refuses
    /// too, rather than appending bytes after the gap the dropped write left
    /// (a silently misaligned stream is exactly what the sticky flag exists to
    /// prevent). A refusal does not itself set `overflowed` — the caller asked
    /// to be told instead — but it does count toward [`wanted`](Self::wanted),
    /// so a retry sized from `wanted` covers these bytes as well.
    pub fn emit_checked(&mut self, bytes: &[u8]) -> bool {
        self.wanted += bytes.len();
        if self.overflowed || self.len + bytes.len() > self.capacity {
            return false;
        }
        let _write = platform::JitWriteScope::enter();
        // SAFETY: `len + bytes.len() <= capacity` was checked above, `ptr` is this buffer's own writable mapping of `capacity` bytes, and `&mut self` rules out `bytes` aliasing it.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(self.len), bytes.len());
        }
        self.len += bytes.len();
        true
    }

    /// Emit a single byte.
    ///
    /// Marks the buffer [`overflowed`](Self::overflowed) and skips the write
    /// instead of panicking when capacity is exhausted. Like [`emit`](Self::emit),
    /// honors the sticky `overflowed` flag: a no-op once the buffer has
    /// already overflowed.
    #[inline]
    pub fn emit_byte(&mut self, b: u8) {
        self.wanted += 1;
        if self.overflowed || self.len >= self.capacity {
            self.overflowed = true;
            return;
        }
        let _write = platform::JitWriteScope::enter();
        // SAFETY: `len < capacity` was checked above and `ptr` is this buffer's own writable mapping of `capacity` bytes.
        unsafe {
            *self.ptr.add(self.len) = b;
        }
        self.len += 1;
    }

    /// Returns `true` if any `emit`/`emit_byte` call exceeded capacity.
    /// When set, the emitted code is incomplete and must be discarded.
    #[inline]
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Force the buffer into the [`overflowed`](Self::overflowed) state.
    ///
    /// Reserved for a genuine CAPACITY problem — a write that did not fit, or a
    /// patch offset past the emitted length. A codegen site that discards the
    /// method for any OTHER reason must use
    /// [`mark_codegen_unencodable`](Self::mark_codegen_unencodable) instead:
    /// see that method for why the distinction is load-bearing.
    #[inline]
    pub fn mark_overflowed(&mut self) {
        self.overflowed = true;
    }

    /// Discard the method because codegen hit a hard invariant it cannot encode
    /// — a `rel8`/`rel32` displacement out of range, a frame offset with no
    /// ModRM form, a deopt stub whose frame reserved no register-save area.
    ///
    /// These sites used to call [`mark_overflowed`](Self::mark_overflowed),
    /// which made the driver report every one of them as
    /// *"code buffer estimate too small; retrying at the measured size"*. That
    /// was wrong twice over:
    ///
    /// * **It named the wrong defect.** The buffer was not too small; `wanted`
    ///   on such a bail is typically well UNDER `capacity`, so the printed
    ///   diagnostic contradicted itself and no reader could tell which of the
    ///   ten sites had fired.
    /// * **It retried forever.** The overflow bail is the one bail
    ///   `try_compile` exempts from the permanent bail list, on the theory that
    ///   the next attempt allocates from a measurement instead of the
    ///   heuristic. But the hint is derived from `wanted`, and when `wanted` is
    ///   below the heuristic the recomputed size is IDENTICAL — so the method
    ///   was re-lowered in full, and failed in exactly the same place, on every
    ///   warmup-gate re-attempt for the life of the process, while never
    ///   becoming compiled.
    ///
    /// A bigger buffer cannot fix any of these, so they are permanent: the
    /// driver reports `reason` by name and lets `try_compile` bail-list the
    /// method after ONE attempt.
    ///
    /// Also sets `overflowed`, so every existing
    /// `if buf.overflowed() { return None; }` discard keeps working unchanged.
    #[inline]
    pub fn mark_codegen_unencodable(&mut self, reason: &'static str) {
        if self.codegen_failed.is_none() {
            self.codegen_failed = Some(reason);
        }
        self.overflowed = true;
    }

    /// The named codegen invariant that discarded this method, if the discard
    /// was NOT a capacity problem. `None` means a genuine buffer overflow (or
    /// no failure at all — check [`overflowed`](Self::overflowed) first).
    #[inline]
    pub fn codegen_failure_reason(&self) -> Option<&'static str> {
        self.codegen_failed
    }

    /// Current write position (offset from start).
    #[inline]
    pub fn pos(&self) -> usize {
        self.len
    }

    /// Total bytes codegen asked to emit, including writes dropped after an
    /// overflow. On an overflowed buffer this is the capacity the compile
    /// actually needed (a lower bound: a dropped write still advances it, and
    /// `rewind_to` does not take bytes back off it).
    #[inline]
    pub fn wanted(&self) -> usize {
        self.wanted
    }

    /// Rewind the write position back to a previously recorded `pos()`.
    ///
    /// Used to discard speculatively-emitted code — e.g. when
    /// `try_emit_inline` abandons a partially-emitted callee body and
    /// falls back to a normal call. Bytes past `offset` are left as-is
    /// in the backing allocation (they will be overwritten by the next
    /// `emit`); only `len` moves. Rewinding *forward* (offset > len) is
    /// rejected so a stale checkpoint cannot expose uninitialized bytes.
    #[inline]
    pub fn rewind_to(&mut self, offset: usize) {
        if offset <= self.len {
            self.len = offset;
        }
    }

    /// Get a pointer to the start of the executable code.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    /// Size of the private executable allocation backing this method.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get the emitted bytes as a slice.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is this buffer's live mapping, which the OS zero-fills, and `len <= capacity` always holds.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Patch 4 bytes (little-endian i32) at the given offset.
    ///
    /// On out-of-bounds offset, marks the buffer
    /// [`overflowed`](Self::overflowed) and returns
    /// `Err(CompileError::PatchFailed)` instead of panicking. The surrounding
    /// compile driver inspects `overflowed()` after codegen and bails to the
    /// interpreter; the caller may ignore the `Err` and rely on that bail.
    pub fn try_patch_i32(&mut self, offset: usize, value: i32) -> Result<(), CompileError> {
        if offset.checked_add(4).map_or(true, |end| end > self.len) {
            // Log only the FIRST overflow for this buffer. `overflowed` is
            // sticky, so every later patch in the same compile hits this arm
            // too; one oversized method used to emit tens of thousands of
            // identical lines. The actionable diagnostic (method, code_len,
            // capacity, wanted) is logged once per method by the compile
            // driver's "code buffer estimate too small" bail.
            if !self.overflowed {
                tracing::warn!(
                    offset = offset,
                    len = self.len,
                    capacity = self.capacity,
                    wanted = self.wanted,
                    buffer = self.tag,
                    "JIT try_patch_i32: offset out of bounds; marking buffer overflowed"
                );
            }
            self.overflowed = true;
            return Err(CompileError::PatchFailed {
                kind: "i32",
                offset,
            });
        }
        let bytes = value.to_le_bytes();
        let _write = platform::JitWriteScope::enter();
        // Safety: bounds checked above; ptr is owned and writable.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(offset), 4);
        }
        Ok(())
    }

    /// Patch 1 byte at the given offset.
    ///
    /// On out-of-bounds offset, marks the buffer
    /// [`overflowed`](Self::overflowed) and returns
    /// `Err(CompileError::PatchFailed)` instead of panicking. See
    /// [`try_patch_i32`](Self::try_patch_i32) for the bail-out contract.
    pub fn try_patch_byte(&mut self, offset: usize, value: u8) -> Result<(), CompileError> {
        if offset >= self.len {
            // First overflow only; see `try_patch_i32` for why.
            if !self.overflowed {
                tracing::warn!(
                    offset = offset,
                    len = self.len,
                    capacity = self.capacity,
                    wanted = self.wanted,
                    buffer = self.tag,
                    "JIT try_patch_byte: offset out of bounds; marking buffer overflowed"
                );
            }
            self.overflowed = true;
            return Err(CompileError::PatchFailed {
                kind: "byte",
                offset,
            });
        }
        let _write = platform::JitWriteScope::enter();
        // Safety: bounds checked above.
        unsafe {
            *self.ptr.add(offset) = value;
        }
        Ok(())
    }

    /// Patch a `rel8` branch displacement, or discard the compile.
    ///
    /// The one rule this enforces is that a displacement which does not fit in
    /// an `i8` must NEVER be written truncated. `rel as u8` is not a near-miss:
    /// it turns an out-of-range forward branch into a backward one, and on x86
    /// the landing site is normally the middle of an earlier instruction. The
    /// inline-PIC cascade shipped exactly that — `JNE -128` into the body of
    /// the pre-call spill/shadow-push run, which then ran as an unguarded
    /// infinite push loop (`jit-raw-jit-to-jit-shadow-stack-
    /// overflow-FIXED-20260731.md`), and the same wrap reappeared as a
    /// deterministic SIGILL in the `CRATONVM_NO_MOVING_YOUNG=1` lane, whose
    /// larger slot bodies pushed the same branch past 127 bytes.
    ///
    /// Marking the buffer overflowed makes the driver's
    /// `if buf.overflowed() { return None; }` discard the half-emitted method
    /// and fall back to the interpreter, which is always valid. Every emitter
    /// that patches a short branch goes through here — `jit`'s only remaining
    /// `try_patch_byte(.., .. as u8)` is the one inside this function, which
    /// `rel8_displacement_patches_all_go_through_the_range_checked_helper`
    /// pins.
    pub fn patch_rel8_or_bail(&mut self, patch: usize, rel: i64) {
        match i8::try_from(rel) {
            // Cast: rel8 displacement, range-checked immediately above.
            Ok(v) => {
                self.try_patch_byte(patch, v as u8).ok();
            }
            Err(_) => self.mark_codegen_unencodable("rel8-displacement-out-of-range"),
        }
    }

    /// Erase an already-emitted, straight-line byte range so it costs (almost)
    /// nothing to execute, without moving any byte that follows it.
    ///
    /// Both backends emit the shadow-stack thread fetch unconditionally and
    /// then discover, after the body is lowered, that the method published
    /// nothing and the fetch is dead. Neither can *remove* it: every recorded
    /// offset downstream — branch patches, deopt points, oop-map native PCs —
    /// is already keyed on the current layout. So the range is overwritten in
    /// place.
    ///
    /// Overwriting it with `0x90` alone is not enough. A one-byte `NOP` is
    /// still an instruction that has to be fetched, decoded and retired, and
    /// this range sits on the ENTRY path of the method — a ~46-byte fetch
    /// becomes 46 NOPs executed on every single invocation. On a small, hot,
    /// call-heavy method that is the dominant cost: it is what made
    /// `CratonBench fib` run ~2x slower on the IR tier than the single-pass
    /// body it displaced. Jumping over the range instead retires two bytes.
    ///
    /// The caller must guarantee the range is straight-line and that nothing
    /// branches INTO its interior — landing on `start + 1` would decode the
    /// `rel8` displacement as an opcode. Both current callers emit the range
    /// as one unbroken prologue unit, before any bytecode is lowered.
    pub fn erase_range_with_jump_over(&mut self, start: usize, end: usize) {
        if end <= start {
            return;
        }
        let len = end - start;
        // `JMP rel8` is 2 bytes, so it needs 2 bytes to live in, and the
        // displacement over the remainder must fit in an `i8`.
        if len >= 2 && (len - 2) <= 127 && self.try_patch_byte(start, 0xEB).is_ok() {
            // `len - 2 <= 127` is established above; the helper is here so no
            // rel8 displacement in this crate is written by a hand-rolled cast
            // (see `patch_rel8_or_bail`).
            self.patch_rel8_or_bail(start + 1, (len - 2) as i64);
            // The skipped bytes are unreachable now, so their encoding is
            // irrelevant; `0x90` keeps a disassembly dump readable.
            for off in (start + 2)..end {
                let _ = self.try_patch_byte(off, 0x90);
            }
            return;
        }
        for off in start..end {
            let _ = self.try_patch_byte(off, 0x90);
        }
    }

    // task #44: the deprecated panicking `patch_i32` / `patch_byte` shims
    // have been removed. Every internal codegen site was migrated to the
    // `try_patch_*` variants in task #20 (commit acd57f2). A workspace grep
    // confirmed zero remaining callers before deletion; the `try_*`
    // variants are the only entry points now.

    /// Read 4 bytes (little-endian i32) at the given offset.
    pub fn read_i32(&self, offset: usize) -> i32 {
        assert!(offset + 4 <= self.len, "read out of bounds");
        let mut bytes = [0u8; 4];
        // SAFETY: the assert above bounds `offset + 4` by `len`, inside this buffer's live mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.add(offset), bytes.as_mut_ptr(), 4);
        }
        i32::from_le_bytes(bytes)
    }

    /// Transition the buffer from writable to executable.
    ///
    /// Calls the OS API to switch from RW to RX permissions.
    ///
    /// After calling `finalize`, writes via `emit`/`emit_byte`/`try_patch_i32` are
    /// undefined behavior. Call [`make_writable`](Self::make_writable)
    /// first if you need to patch code after finalization.
    ///
    /// ABORTS the process if the OS refuses the flip. Prefer
    /// [`try_finalize`](Self::try_finalize) (via `CompiledMethod::try_new`) on
    /// any path that can decline to the interpreter instead.
    pub fn finalize(&self) {
        self.try_finalize().unwrap_or_else(|e| {
            eprintln!("FATAL: JIT: make_executable failed: {e}");
            std::process::abort();
        });
    }

    /// [`finalize`](Self::finalize) without the abort (round 9 wave 2).
    ///
    /// `mprotect`/`VirtualProtect` can refuse for reasons that say nothing
    /// about the process's health — Linux `ENOMEM` at `vm.max_map_count`
    /// (every arena block flip splits the region's VMA), or a policy denial
    /// (SELinux `execmem`, Windows ACG). A compile that gets `Err` here should
    /// discard its artifact and decline to the interpreter, exactly as it does
    /// on an allocation failure; the buffer is still RW and unpublished, so
    /// dropping it is an ordinary unmap (an arena block is restored to RW by
    /// the arena's own release).
    ///
    /// On success the code-stream epoch is advanced exactly as `finalize`
    /// always did; on failure it is not (no bytes became executable).
    pub fn try_finalize(&self) -> Result<(), platform::JitError> {
        platform::make_executable(self.ptr, self.capacity)?;
        // The instruction bytes now exist and are executable, and no entry
        // pointer into them has been published yet (publication follows the
        // `CompiledMethod` constructor that called this). That is exactly the
        // window the reader-side barrier epoch must move in — see
        // `note_code_stream_published` for the stale-stream case the
        // registration-time bump alone missed.
        crate::note_code_stream_published();
        Ok(())
    }

    /// Transition the buffer back from executable to writable (for patching).
    ///
    /// Calls the OS API to switch from RX to RW permissions.
    pub fn make_writable(&self) {
        platform::make_writable(self.ptr, self.capacity).unwrap_or_else(|e| {
            eprintln!("FATAL: JIT: make_writable failed: {e}");
            std::process::abort();
        });
    }
}

/// Whether to trace code-buffer unmaps (`CRATONVM_DBG_JIT_UNMAP`). Read once and
/// cached: `Drop` runs on compile threads and during teardown, where a
/// per-call environment read would be both hot and needlessly fallible.
fn dbg_jit_unmap_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_JIT_UNMAP")
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

impl ExecutableBuffer {
    /// Move this buffer's mapping into a fresh value and leave `self` empty, so
    /// the rest of `self`'s destructor is a no-op.
    ///
    /// An exhaustive struct literal rather than `ptr::read`: a field added
    /// later fails to compile here instead of being silently duplicated.
    fn detach_mapping(&mut self) -> ExecutableBuffer {
        let moved = ExecutableBuffer {
            ptr: self.ptr,
            len: self.len,
            capacity: self.capacity,
            codegen_failed: self.codegen_failed,
            overflowed: self.overflowed,
            wanted: self.wanted,
            published: self.published,
            tag: self.tag,
        };
        self.ptr = std::ptr::null_mut();
        moved
    }
}

impl Drop for ExecutableBuffer {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        // THE INVARIANT, ENFORCED RATHER THAN AUDITED. A published body's
        // mapping may be returned to the OS only from a reclamation the
        // retirement queue proved safe. A last owner dropped anywhere else —
        // a local `Arc` that outlived the cache's withdrawal of the body, a
        // dispatch memo, a per-thread cache — used to unmap right here, with
        // no question asked about whether a thread was still executing inside
        // (`jit-code-uaf-outside-retirement-queue-FIXED-20260918.md`: a C2
        // compile task's `superseded_body` local freed ~27 published C1 bodies
        // per Spring test process this way, and one of them was being executed).
        //
        // Such a drop is now RESCUED: the mapping, still registered and still
        // intact, goes into the retirement queue and is unmapped only once the
        // queue's quiescence proof covers it. The audit keeps counting these —
        // each still names a holder that should have released through the
        // queue — but a stray drop can no longer become a use-after-free.
        if self.published && !reclaim_is_authorised() {
            let n = UNQUEUED_PUBLISHED_CODE_FREES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 32 && dbg_jit_code_free_enabled() {
                eprintln!(
                    "[jit-unqueued-free] PUBLISHED body base={:#x} len={:#x} \
released OUTSIDE the retirement queue -- rescued into it; active_jit_executions={}\n{}",
                    self.ptr as usize,
                    self.capacity,
                    ACTIVE_JIT_EXECUTIONS.get(),
                    std::backtrace::Backtrace::force_capture(),
                );
            }
            defer_orphaned_mapping(self.detach_mapping());
            return;
        }
        // Poison is recovered here too. Skipping the deregistration would leave
        // the region validating pointers into memory unmapped below.
        jit_code_regions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .deregister(self.ptr);
        // Withdraw debugger symbols before the address can be reused.
        crate::code_events::retire(self.ptr as usize, self.capacity);
        COMMITTED_JIT_CODE_BYTES.fetch_sub(self.capacity, std::sync::atomic::Ordering::Relaxed);
        // DIAG: `CRATONVM_DBG_JIT_UNMAP=1` names every code buffer as it is
        // unmapped. Paired with `CRATONVM_DBG=jitc` (which prints each
        // artifact's `entry=0x..`) and the crash handler's `pc=` line, it
        // answers "did this SIGSEGV jump into a body that had just been
        // retired?" in a single run — the question each round of the
        // retired-JIT-code family previously needed a bespoke LD_PRELOAD shim
        // for. It is what identified the unrooted OSR direct-call targets
        // (`osr_direct_callee_entries` in the interpreter's
        // `compile_osr_artifact`).
        if dbg_jit_unmap_enabled() {
            eprintln!(
                "[jit-unmap] ptr=0x{:x} cap={} tid={:?}",
                self.ptr as usize,
                self.capacity,
                std::thread::current().id()
            );
        }
        // Record the unmap BEFORE it happens, so a thread that faults inside
        // this range can be told it was executing freed code (see
        // `recent_code_free_covering`, which the crash handler prints). The
        // active-execution count is what makes the report actionable: a
        // non-zero value means the buffer was released while at least one
        // thread was inside compiled code — which is precisely the bug the
        // `defer_jit_owner` retirement queue above exists to prevent, so a
        // non-zero value here means some release path is still bypassing it.
        let authorised = reclaim_is_authorised();
        let mut flags = 0usize;
        if self.published {
            flags |= CODE_FREE_PUBLISHED;
            PUBLISHED_CODE_FREES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // The reclaim half of `mark_published`'s install accounting.
            RECLAMATION
                .reclaimed_bodies
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            RECLAMATION
                .reclaimed_bytes
                .fetch_add(self.capacity as u64, std::sync::atomic::Ordering::Relaxed);
            RECLAMATION
                .reclaimed_code_bytes
                .fetch_add(self.len as u64, std::sync::atomic::Ordering::Relaxed);
            // Unreachable for `!authorised`: the rescue at the top of this
            // function sent every unauthorised published release to the queue,
            // which frees it under `AuthorisedReclaim`.
            debug_assert!(authorised, "unauthorised published release escaped the rescue");
        }
        if authorised {
            flags |= CODE_FREE_AUTHORISED;
        }
        record_code_free(
            self.ptr as usize,
            self.capacity,
            ACTIVE_JIT_EXECUTIONS.get(),
            flags,
        );
        if never_free_code_enabled() {
            return;
        }
        platform::free_executable(self.ptr, self.capacity);
    }
}

/// DIAG: `CRATONVM_JIT_NEVER_FREE_CODE=1` never unmaps an executable buffer,
/// for ANY owner — unlike [`jit_leak_code_enabled`], which only defers the
/// owners routed through [`defer_jit_owner`]. If a SIGSEGV vanishes under this
/// flag but survives `CRATONVM_JIT_LEAK_CODE=1`, the use-after-free is on a
/// release path the retirement queue does not cover. Leaks every retired body;
/// diagnosis only, never a shipping mode.
fn never_free_code_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_NEVER_FREE_CODE")
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::ExecutableBuffer;

    /// Once a write has been dropped, `emit_checked` refuses as well: a byte
    /// appended after the gap the dropped write left would sit at the wrong
    /// offset in a stream every recorded patch site is keyed on.
    #[test]
    fn emit_checked_honours_the_sticky_overflow() {
        let mut buf = ExecutableBuffer::new(4).expect("alloc");
        buf.emit(&[1, 2, 3]);
        buf.emit(&[4, 5]); // does not fit: sticky overflow, `len` frozen at 3
        assert!(buf.overflowed());
        // One byte of room is left and a one-byte write would fit, but it
        // would land where the dropped write's first byte belonged.
        assert!(!buf.emit_checked(&[9]));
        assert_eq!(buf.pos(), 3);
        assert_eq!(buf.as_slice(), &[1, 2, 3]);
    }

    /// A refused `emit_checked` is reported to its caller, not recorded as an
    /// overflow — but it is still demand the buffer could not meet, so a retry
    /// sized from `wanted` must see it.
    #[test]
    fn emit_checked_counts_toward_wanted_without_poisoning() {
        let mut buf = ExecutableBuffer::new(4).expect("alloc");
        assert!(buf.emit_checked(&[1, 2]));
        assert!(!buf.emit_checked(&[3, 4, 5]));
        assert_eq!(buf.wanted(), 5);
        assert!(!buf.overflowed());
        assert!(buf.emit_checked(&[6, 7]));
        assert_eq!(buf.as_slice(), &[1, 2, 6, 7]);
    }

    /// `finalize` moves the code-region epoch the AArch64 reader-side barrier
    /// is gated on, AFTER the bytes exist. Only "it rose" is asserted: other
    /// tests allocate and finalize concurrently, and the epoch only rises.
    #[test]
    fn finalize_advances_the_code_stream_epoch() {
        let mut buf = ExecutableBuffer::new(64).expect("alloc");
        buf.emit(&[0xC3]);
        let before = crate::code_ptr_regions_epoch();
        buf.finalize();
        assert!(crate::code_ptr_regions_epoch() > before);
    }

    /// A refused RW->RX flip is an `Err`, not an abort, and a buffer whose
    /// flip was refused is still writable and droppable (round 9 wave 2).
    #[test]
    fn try_finalize_reports_a_refused_protect_instead_of_aborting() {
        let mut buf = ExecutableBuffer::new(64).expect("alloc");
        buf.emit(&[0xC3]);
        crate::platform::fail_next_make_executable_for_test(12); // ENOMEM
        match buf.try_finalize() {
            Err(crate::platform::JitError::ProtectFailed(12)) => {}
            other => panic!("expected ProtectFailed(12), got {other:?}"),
        }
        // Still RW: a write after the refused flip must not fault.
        buf.emit(&[0x90]);
        assert_eq!(buf.as_slice(), &[0xC3, 0x90]);
        // The injection is one-shot: the next flip succeeds.
        assert!(buf.try_finalize().is_ok());
    }
}
