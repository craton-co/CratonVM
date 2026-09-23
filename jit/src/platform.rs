// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Platform-specific executable memory allocation.
//!
//! Abstracts over OS-level APIs for allocating memory that can hold and execute
//! machine code. All platforms enforce W^X (write xor execute):
//!
//! - **Windows:** `VirtualAlloc` with `PAGE_READWRITE`, then `VirtualProtect`
//!   to `PAGE_EXECUTE_READ` when code is finalized. On non-x86 Windows
//!   (`aarch64-pc-windows-msvc`, `arm64ec-*`) the I-cache is *not* coherent
//!   with the D-cache, so `make_executable` additionally calls
//!   `FlushInstructionCache` — see [`flush_icache_range_windows`]. This was
//!   missing until the 2026-08-01 aarch64 parity audit: the Unix path had an
//!   arch-conditional `__clear_cache` (added when the Linux/FreeBSD aarch64
//!   port landed) but the Windows path had none, so a Windows-on-ARM build
//!   published every JIT body without ever invalidating the I-cache.
//! - **Every other Unix (Linux, Android, the BSDs, illumos):** `mmap` with
//!   `PROT_READ | PROT_WRITE`, then `mprotect` to `PROT_READ | PROT_EXEC`. On
//!   every architecture *except* x86/x86-64 we additionally flush the
//!   instruction cache via the compiler builtin `__clear_cache` before the
//!   RW→RX flip, because those I-caches and D-caches are not coherent (unlike
//!   x86-64, which is automatically coherent and needs no flush). That gate is
//!   an architecture EXCLUSION, matching the Windows arm; it used to be an
//!   allow-list of `linux`/`freebsd`, which silently omitted
//!   `aarch64-linux-android` (whose `target_os` is `"android"`), the ARM64
//!   BSDs, 32-bit ARM and RISC-V — see [`flush_icache_range_unix`].
//!
//! All three of those are the WRITER's half of publishing code. The reader's
//! half — a context-synchronisation event on the core that is about to fetch
//! the new instructions — is [`synchronize_instruction_stream_for_epoch`], and
//! that is the one a porter should call: every production reader-side barrier in
//! this workspace goes through it (`jit/src/lib.rs`, twice), because the epoch
//! argument is what keeps the barrier from being paid once per dispatch, and the
//! ratio of issued to skipped "is the whole argument for the epoch gate".
//! [`synchronize_instruction_stream`] is its unconditional implementation — the
//! body it runs once the epoch comparison says a barrier is owed — and calling
//! it directly is correct but skips that gate.
//!
//! Either way, every platform that flips page protection gets the
//! synchronisation event by accident (the TLB-shootdown IPI is an interrupt,
//! hence a synchronisation event), and macOS/arm64, which never flips a
//! protection, does not.
//! - **macOS ARM64 (Apple Silicon):** W^X is enforced PER THREAD on `MAP_JIT`
//!   pages, not per page. The region is mapped once as RWX with `MAP_JIT`, and
//!   `pthread_jit_write_protect_np` switches the calling thread between
//!   "may write, may not execute" and the reverse. `mprotect` is not used: the
//!   hardened runtime refuses to make a once-writable `MAP_JIT` page executable
//!   that way, so the previous RW-then-`mprotect` shape aborted every compile
//!   on a hardened build. Every write to code memory therefore runs inside a
//!   [`JitWriteScope`]. I-cache flush uses Apple's `sys_icache_invalidate`.
//!
//! The Unix `mmap` flags are NOT portable: `MAP_ANONYMOUS` is `0x20` on Linux
//! and `0x1000` on the BSDs and Darwin. See [`map_anonymous`].

/// Errors that can occur during JIT memory operations.
///
/// One variant, and that is not an oversight. The only operation in this module
/// that can fail with a *reason worth classifying* is the protection flip.
/// **Allocation** failure is reported by `Option::None` from
/// [`alloc_executable`] and counted as
/// [`crate::code_alloc_failure::OS_REFUSED`]; it does not travel as a
/// `JitError`. There used to be an `AllocationFailed` variant for it, deleted in
/// round 10 wave 8 because nothing in the workspace could construct it — the
/// allocation door returns `Option<*mut u8>` and has no `JitError` to return —
/// so its only two constructions were the two tests that asserted properties of
/// it, which reads as coverage of a path that cannot occur. If an allocation
/// failure is ever routed through this type, it has to agree with
/// `note_jit_code_alloc_failure`'s existing accounting rather than become a
/// second, silent one; see
/// `docs/internal/fixed-bugs/r10-readers-platform-dead-public-surface-RESOLVED-20260922.md`
/// item 3.
#[derive(Debug)]
pub enum JitError {
    /// mprotect / VirtualProtect failed to change page permissions. The payload
    /// is the OS error code (`errno` on Unix, `GetLastError()` on Windows), not
    /// the call's return value, which is always the uninformative `-1` / `0`.
    ProtectFailed(i32),
}

/// The `errno` of the call that just failed, for [`JitError::ProtectFailed`].
///
/// `mprotect` returns `-1` for every failure, so recording its return value --
/// what this module used to do -- told a reader nothing about whether the
/// mapping was wrong (`EINVAL`), the policy refused (`EACCES`) or memory ran
/// out (`ENOMEM`). `-1` survives only when the OS reports no code at all.
#[cfg(not(target_os = "windows"))]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), allow(dead_code))]
fn last_os_error_code() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

/// Which `mmap(2)` flag values an operating system uses.
///
/// `PROT_*`, `MAP_PRIVATE` and `MAP_FAILED` agree across every Unix this crate
/// can target; `MAP_ANONYMOUS` does not. The Unix allocator hard-coded Linux's
/// `0x20` for "Linux, FreeBSD, macOS x86-64" alike. On FreeBSD and Darwin
/// `0x20` is `MAP_RENAME` (or unassigned), so `mmap` failed with `EINVAL`,
/// `alloc_executable` returned `None`, and the JIT was silently dead there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // a Windows build names no Unix ABI
enum UnixMmapAbi {
    /// Linux and Android on every architecture but MIPS.
    Linux,
    /// Linux on MIPS, which kept the IRIX flag values.
    LinuxMips,
    /// Darwin, FreeBSD, NetBSD, OpenBSD, DragonFly.
    Bsd,
    /// illumos and Solaris.
    Solaris,
}

/// `MAP_ANONYMOUS` for `abi`. A pure function of its argument so the table is
/// checked on every host, not only on the one it describes.
#[allow(dead_code)] // unused on Windows
const fn map_anonymous(abi: UnixMmapAbi) -> i32 {
    match abi {
        UnixMmapAbi::Linux => 0x20,
        UnixMmapAbi::LinuxMips => 0x800,
        UnixMmapAbi::Bsd => 0x1000,
        UnixMmapAbi::Solaris => 0x100,
    }
}

#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    ))
))]
const HOST_MMAP_ABI: Option<UnixMmapAbi> = Some(UnixMmapAbi::Linux);
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    )
))]
const HOST_MMAP_ABI: Option<UnixMmapAbi> = Some(UnixMmapAbi::LinuxMips);
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
#[allow(dead_code)] // macOS/ARM64's allocator names `UnixMmapAbi::Bsd` directly
const HOST_MMAP_ABI: Option<UnixMmapAbi> = Some(UnixMmapAbi::Bsd);
#[cfg(any(target_os = "illumos", target_os = "solaris"))]
const HOST_MMAP_ABI: Option<UnixMmapAbi> = Some(UnixMmapAbi::Solaris);
/// Any other OS: no known flag value, so the allocator refuses rather than
/// passing a guess to `mmap`. Refusing is what the wrong constant did too, but
/// now it is the documented outcome rather than an accident.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "illumos",
    target_os = "solaris"
)))]
#[allow(dead_code)] // Windows never maps through `mmap`
const HOST_MMAP_ABI: Option<UnixMmapAbi> = None;

/// A region in which the CURRENT THREAD may write into JIT code memory.
///
/// A zero-cost no-op everywhere except macOS on Apple Silicon, where code pages
/// are `MAP_JIT` and executable-or-writable per thread. There, entering the
/// first scope on a thread calls `pthread_jit_write_protect_np(0)` and leaving
/// the last calls `pthread_jit_write_protect_np(1)`. Scopes nest, and the
/// thread is back in execute mode as soon as the outermost one drops.
///
/// Hold it only around the write itself. While it is held, EVERY `MAP_JIT`
/// page is non-executable for this thread, so calling into compiled code from
/// inside a scope faults. That is why `ExecutableBuffer` takes a scope per
/// copy rather than for the life of a buffer: a buffer that is allocated and
/// then abandoned without `finalize` (every failed compile) would otherwise
/// leave its thread unable to run any JIT code again.
///
/// `!Send`, because the permission belongs to the thread that entered it.
#[must_use = "the write permission lasts only as long as the scope"]
pub struct JitWriteScope {
    _thread_bound: std::marker::PhantomData<*const ()>,
}

impl JitWriteScope {
    /// Allow this thread to write code memory until the scope drops.
    #[inline]
    pub fn enter() -> Self {
        platform_jit_write_begin();
        Self {
            _thread_bound: std::marker::PhantomData,
        }
    }
}

impl Drop for JitWriteScope {
    #[inline]
    fn drop(&mut self) {
        platform_jit_write_end();
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[inline(always)]
fn platform_jit_write_begin() {}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[inline(always)]
fn platform_jit_write_end() {}

impl std::fmt::Display for JitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JitError::ProtectFailed(code) => write!(f, "JIT memory protect failed (code {})", code),
        }
    }
}

impl std::error::Error for JitError {}

impl JitError {
    /// Whether this refusal is a PROCESS-WIDE policy denial of executable
    /// memory, as opposed to a per-call condition (round 9 wave 4,
    /// `executable-buffer-finalize-aborts-the-process-on-protect-failure`).
    ///
    /// A policy denial -- SELinux/PaX `execmem` (`EACCES`), a hardened or
    /// sandboxed runtime (`EPERM`), Windows Arbitrary Code Guard
    /// (`ERROR_DYNAMIC_CODE_BLOCKED`) -- will refuse EVERY later flip in this
    /// process, so the caller may latch "the JIT is unavailable" instead of
    /// paying a whole compile per method to rediscover it. Anything else,
    /// notably Linux `ENOMEM` at `vm.max_map_count` (the arena's VMA splits),
    /// is transient: that one compile declines and the next may succeed.
    ///
    /// Allocation failures cannot reach here at all: they are never policy
    /// (`alloc_executable` maps RW only, which no `execmem` policy refuses) and
    /// since round 10 wave 8 they have no variant to arrive in either — see the
    /// type's own doc.
    pub fn is_policy_denial(&self) -> bool {
        match *self {
            JitError::ProtectFailed(code) => protect_code_is_policy_denial(code),
        }
    }
}

/// See [`JitError::is_policy_denial`]. `ERROR_DYNAMIC_CODE_BLOCKED` is 1655.
/// `ERROR_ACCESS_DENIED` (5) is deliberately NOT here: `VirtualProtect`
/// reports it for ordinary range/state mistakes too, and a false latch would
/// switch the JIT off for the life of the process.
#[cfg(target_os = "windows")]
fn protect_code_is_policy_denial(code: i32) -> bool {
    code == 1655
}

/// See [`JitError::is_policy_denial`]. `EPERM` is 1 and `EACCES` is 13 on
/// every Unix this crate targets. For an anonymous private mapping this crate
/// created, `mprotect` returns them only when a security policy refuses
/// `PROT_EXEC`; `EINVAL`/`ENOMEM` are the per-call failures.
#[cfg(not(target_os = "windows"))]
fn protect_code_is_policy_denial(code: i32) -> bool {
    code == 1 || code == 13
}

/// Allocate `size` bytes of memory suitable for writing machine code.
///
/// The returned pointer is guaranteed to be writable but NOT executable.
/// Call [`make_executable`] after writing code to enable execution.
///
/// A `None` is counted in the process code-cache allocation ledger as
/// [`crate::code_alloc_failure::OS_REFUSED`], which is what makes
/// `vm::jit::code_cache_lifecycle`'s `failed_allocations` able to leave zero.
/// This is the RIGHT place for that call rather than
/// `crate::ExecutableBuffer::new`: this function is the crate's single door to
/// the OS mapping primitive for code, so the count stays correct for the
/// trampoline and stub allocations that do not go through an
/// `ExecutableBuffer`, and cannot drift if another buffer constructor is added.
/// The arena path does NOT come through here (see [`crate::ExecutableBuffer::new_in`],
/// which records its own refusal), so nothing is double-counted.
///
/// A zero-size request is counted too. It is refused by every backing
/// allocator and by [`JitCodeArena::alloc`]'s explicit guard, so it is a real
/// refusal; it is also a caller bug rather than memory pressure, and the only
/// occurrences in this workspace are in this file's own tests. It is left
/// counted rather than special-cased because an untaken branch that exists to
/// hide a number is how a gauge starts lying.
pub fn alloc_executable(size: usize) -> Option<*mut u8> {
    let p = platform_alloc(size);
    if p.is_none() {
        crate::note_jit_code_alloc_failure(size, crate::code_alloc_failure::OS_REFUSED);
    }
    p
}

/// Free executable memory previously allocated by [`alloc_executable`] — or
/// return an arena block to its [`JitCodeArena`].
///
/// # Why the arena is dispatched on the ADDRESS and not on a back-pointer
///
/// The natural shape is an `Option<Arc<Mutex<JitCodeArena>>>` field on
/// `ExecutableBuffer`, which its `Drop` would consult. That field could not be
/// added from this module when the arena landed — `ExecutableBuffer` and its
/// `Drop` then lived in `jit/src/lib.rs`, which that change was not permitted
/// to edit (they have since moved to `jit/src/exec_memory.rs`, which is where
/// the field would go) — so the back-pointer is instead held by a process-wide
/// span table (`ARENA_SPANS`), and the buffer's identity as an arena block is
/// recovered from its base address. See the `// REVIEW-NOTE:` at the bottom of
/// this file for the follow-up that replaces this with the field.
///
/// The dispatch is not merely a convenience: calling `platform_free` on an
/// address INSIDE a region would be a disaster on both families.
/// `VirtualFree(p, 0, MEM_RELEASE)` requires `p` to be the base the reservation
/// was returned at and fails otherwise (leaking, but harmlessly);
/// `munmap(p, len)` does not — it would punch a hole through the middle of a
/// live region and unmap somebody else's compiled body. So once an address
/// falls inside a known arena span this function NEVER reaches `platform_free`,
/// even when the owning arena has since been dropped and the block can only be
/// leaked.
///
/// Cost when no arena has ever mapped a region (the default, because
/// `CRATONVM_JIT_CODE_ARENA` is off): one `Acquire` load of a counter that is
/// zero, then the original call. See [`jit_code_arena_enabled`].
///
/// `Acquire`, not relaxed as this sentence used to say. [`free_if_arena`] loads
/// `ARENA_SPAN_COUNT` with `Ordering::Acquire`, and that static's own doc
/// explains at length why it has to: the count is the fast-path guard for a
/// table another thread publishes into, so a relaxed read could observe a
/// non-zero count without the span writes that justify it. The cost claim is
/// unaffected on x86-64, where an acquire load is a plain `mov`, but the word
/// mattered enough to fix — a reader trusting "relaxed" could conclude the
/// ordering was free to change.
pub fn free_executable(ptr: *mut u8, size: usize) {
    if free_if_arena(ptr) {
        return;
    }
    platform_free(ptr, size);
}

/// Transition a region from writable to executable.
///
/// This calls the appropriate OS API to switch from RW to RX permissions.
/// After this call, writes to the memory are undefined behavior until
/// [`make_writable`] is called.
pub fn make_executable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    if let Some(code) = injected_make_executable_failure() {
        return Err(JitError::ProtectFailed(code));
    }
    platform_make_executable(ptr, size)
}

// Fault injection for the fallible-finalize path (round 9 wave 2). A real
// protect failure cannot be provoked portably, so unit tests arm a one-shot,
// THREAD-LOCAL failure: other tests running concurrently on other threads are
// unaffected. Compiled out of non-test builds entirely.
#[cfg(test)]
thread_local! {
    static FAIL_NEXT_MAKE_EXECUTABLE: std::cell::Cell<Option<i32>> =
        const { std::cell::Cell::new(None) };
}

/// Test hook: the next [`make_executable`] on THIS thread returns
/// `Err(JitError::ProtectFailed(code))` without touching the mapping.
#[cfg(test)]
pub(crate) fn fail_next_make_executable_for_test(code: i32) {
    FAIL_NEXT_MAKE_EXECUTABLE.with(|c| c.set(Some(code)));
}

#[cfg(test)]
fn injected_make_executable_failure() -> Option<i32> {
    FAIL_NEXT_MAKE_EXECUTABLE.with(|c| c.take())
}

#[cfg(not(test))]
#[inline(always)]
fn injected_make_executable_failure() -> Option<i32> {
    None
}

/// Make a previously-finalized executable region writable again for patching.
///
/// This calls the appropriate OS API to switch from RX back to RW permissions.
pub fn make_writable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    platform_make_writable(ptr, size)
}

/// The **reader-side** half of publishing JIT code: a context-synchronisation
/// event on the CPU that is about to execute it.
///
/// [`make_executable`] is the writer's half. It does everything the writer can
/// do — clean the data cache to the point of unification, invalidate the
/// instruction cache (`IC IVAU` is broadcast to the Inner Shareable domain, so
/// it reaches every core), and `DSB`/`ISB` on the writing PE. What it
/// structurally CANNOT do is the last step ARM ARM B2.4.4 (*Concurrent
/// modification and execution of instructions*) requires: a context
/// synchronisation event on **each other PE** that will fetch those
/// instructions. A PE may hold already-fetched, already-decoded state for that
/// address; only an `ISB`, an exception entry/return, or a similar event on
/// THAT PE discards it.
///
/// Why this has not bitten the ordinary platforms: `mprotect` /
/// `VirtualProtect` change a page's protection, which forces a TLB shootdown,
/// which is delivered as an IPI to every core running the address space, and
/// taking an interrupt IS a context synchronisation event. The requirement is
/// satisfied by accident, on the way to something else.
///
/// **On macOS/arm64 that accident does not happen.** `platform_alloc` maps the
/// region `PROT_READ|PROT_WRITE|PROT_EXEC` with `MAP_JIT` exactly once and
/// `platform_make_executable` is `sys_icache_invalidate` with *no protection
/// change at all* — W^X there is enforced per thread by
/// `pthread_jit_write_protect_np`, which is a thread-local register write, not
/// a cross-core IPI. So on the one platform whose protection flip was
/// deliberately removed, the guarantee every other platform leans on is gone,
/// and a thread other than the compiler can branch into a freshly published
/// `CompiledMethod` having had no synchronisation event on its own PE.
///
/// # Contract for callers
///
/// Call this on a thread that is about to enter, for the first time, a
/// `CompiledMethod` whose code was published by a DIFFERENT thread — i.e. on
/// the dispatch path, after the load that observes the new entry pointer and
/// before the indirect branch to it. It is cheap (a single `ISB` on aarch64,
/// nothing at all on x86-64) and it is idempotent, so erring towards calling it
/// costs a handful of cycles.
///
/// This function deliberately lives here rather than in the dispatcher: the
/// "which instruction does this architecture need" question belongs to the
/// platform layer. The call site is in the VM's dispatch path and is not part
/// of this module.
#[inline]
pub fn synchronize_instruction_stream() {
    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    {
        // `ISB SY` — Instruction Synchronization Barrier, full system. It
        // flushes the pipeline and forces every instruction after it to be
        // re-fetched, so any stale decode of the newly published bytes is
        // discarded. This is the reader-side event ARM ARM B2.4.4 asks for.
        //
        // No `options(nomem)`: we WANT the compiler to treat this as a memory
        // barrier too, so it cannot sink the load of the entry pointer below
        // it. `preserves_flags` is accurate — ISB does not touch NZCV — and
        // `nostack` is accurate because it neither pushes nor calls.
        //
        // SAFETY: `ISB` has no operands, no memory operands and no
        // architectural side effects other than the synchronisation itself; it
        // is unprivileged and legal at EL0.
        unsafe {
            core::arch::asm!("isb sy", options(nostack, preserves_flags));
        }
    }
    // x86/x86-64 need nothing: the caches are coherent (Intel SDM Vol.3 §11.6,
    // AMD APM Vol.2 §7.6.1) and the indirect branch into the new code is itself
    // the serialising operation the cross-modifying-code protocol requires.
    //
    // Any other architecture: no-op rather than a wrong instruction. A port
    // that needs one (RISC-V wants `fence.i` on the executing hart) adds its
    // arm here, in the one place the question is asked.
}

/// The last code-publication epoch this thread has synchronised its
/// instruction stream against.
///
/// `u64::MAX` is the "never" seed rather than `0`, because `0` is a legal
/// epoch: a thread that had never synchronised would otherwise be
/// indistinguishable from one that had synchronised before anything was ever
/// published, and would skip the `ISB` for the first method it ever calls.
#[cfg(not(target_arch = "x86_64"))]
thread_local! {
    static SYNCED_CODE_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}

/// Barriers [`synchronize_instruction_stream_for_epoch`] actually issued.
///
/// Only the ISSUED side is counted, deliberately. The skipped side is every
/// compiled dispatch in the process, and a relaxed `fetch_add` on one shared
/// word from every thread on every call is a contended cacheline — a counter
/// that costs more than the thing it is measuring saves. An issue is rare by
/// construction (once per thread per publication), so this one is free.
///
/// Always zero on x86-64, where the whole mechanism is compiled out.
static ISB_ISSUED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many reader-side instruction-stream barriers this process has issued, or
/// `None` on a target that has no such barrier to issue.
///
/// `Some(n)`: `n` barriers were issued. Expected to be roughly "threads that
/// call compiled code" times "publications", and NOT to scale with the number of
/// calls — that ratio is the whole argument for the epoch gate, and it is the
/// number to look at if dispatch ever measures slower on aarch64 than on x86-64
/// for reasons nothing else explains.
///
/// `None`: **this target has no reader-side barrier at all**, so there is no
/// count, not a count of zero. On x86-64 the instruction stream is coherent,
/// [`synchronize_instruction_stream`] is a no-op and
/// [`synchronize_instruction_stream_for_epoch`] is compiled out entirely, so
/// `ISB_ISSUED` is structurally never incremented.
///
/// # Why this is an `Option` and not a `u64`
///
/// It used to be a `u64`, and on x86-64 it returned `0` — which is the defect
/// round 10 found ten times over in other files, with the polarity that makes it
/// hardest to catch: a reading that is *correct* and uninterpretable. A caller
/// printing `instruction-stream barriers: 0` cannot tell "the epoch gate is
/// working beautifully and every thread was already synchronised" from "this
/// architecture does not have the mechanism" from "the counter is never fed". Two
/// of those three are fine and one is a bug, and the line reads identically for
/// all three.
///
/// So the distinction moved into the type, where a caller cannot fail to see it.
/// Round 10 wave 6 declined to give this accessor a reader in `vm-cli`'s dump for
/// exactly this reason and called the fix "a `cfg`-gated reader, or an explicit
/// `<not applicable on this target>` rather than `0`"; the second of those is
/// what a caller can now write, and it can write it without knowing which
/// architectures have an I-cache coherency problem. That knowledge belongs here.
///
/// **Still has no production caller** — it is a frozen entry in
/// `scripts/baselines/orphan-instruments-allowlist.txt` (`fn
/// instruction_stream_barriers_issued`) and this change does not retire it,
/// because a return type is not a reader. What it does is make the reader
/// writable by someone who is not making an aarch64 bring-up judgement: the
/// remaining work is one line in `vm-cli/src/main.rs`'s method-stats dump, which
/// is another lane's file. See
/// `docs/internal/fixed-bugs/r10-readers-platform-dead-public-surface-RESOLVED-20260922.md`
/// item 4 and `docs/feature-designs/jit-r10-vecclasses-proposals.md`.
pub fn instruction_stream_barriers_issued() -> Option<u64> {
    // `cfg!` rather than `#[cfg]`: `ISB_ISSUED` is declared unconditionally, so
    // both arms compile on every target and the load below cannot rot on the
    // architecture that does not take it.
    if cfg!(target_arch = "x86_64") {
        return None;
    }
    Some(ISB_ISSUED.load(std::sync::atomic::Ordering::Relaxed))
}

/// [`synchronize_instruction_stream`], but at most once per thread per
/// publication epoch.
///
/// # Why an epoch gate and not an unconditional barrier
///
/// The contract on [`synchronize_instruction_stream`] says to call it before
/// first entering code another thread published, and an honest reading of that
/// on a dispatch path is "before every call", because the dispatcher cannot
/// tell a first entry from a millionth. An `ISB` is a full pipeline flush —
/// tens of cycles on a wide core — and `CompiledMethod::try_call` is on the hot
/// path of every compiled invocation, so an unconditional barrier there is a
/// real tax on a workload that publishes nothing.
///
/// The requirement is not per CALL, it is per *newly published instruction
/// stream*: once this PE has taken a context-synchronisation event after the
/// publication, every fetch it makes afterwards sees the new bytes. So the
/// question the gate has to answer is "has anything been published since the
/// last event on this PE", and the JIT already maintains exactly that number —
/// the code-region epoch that `jit_code_regions` bumps on every registration.
///
/// Pass it here. This thread issues one `ISB` the first time it dispatches
/// after any new region appears, and compares two words on every dispatch after
/// that.
///
/// # What the caller owes
///
/// * `epoch` must be **monotone** and must be bumped AFTER the instruction
///   bytes are written and BEFORE the entry pointer of any method in them can
///   be observed. A bump at registration alone is not enough: registration
///   happens before a single byte is emitted, so a thread that dispatches
///   anything while the buffer is still being filled records that value as
///   synchronised and later enters the finished body with no barrier. The
///   JIT's region epoch satisfies the contract because
///   `ExecutableBuffer::finalize` bumps it again (`note_code_stream_published`
///   in `lib.rs`) once the bytes exist and before the `CompiledMethod` carrying
///   the entry is published, so a reader that acquires the method also
///   observes that bump.
/// * Load it with `Acquire` (or stronger) at the call site, for the same
///   reason.
/// * Call it after the load that observes the entry pointer and before the
///   indirect branch, exactly as the unconditional form requires.
///
/// A region registered between this call and the branch is not covered — but it
/// cannot be the target of a branch whose pointer was already resolved, which
/// is the only thing this barrier protects.
///
/// # x86-64
///
/// Compiled out entirely: the base primitive is already a no-op there, so the
/// gate would be a thread-local access guarding nothing.
#[inline]
pub fn synchronize_instruction_stream_for_epoch(epoch: u64) {
    #[cfg(not(target_arch = "x86_64"))]
    {
        SYNCED_CODE_EPOCH.with(|cell| {
            if cell.get() == epoch {
                return;
            }
            // Barrier FIRST, then record. A panic or a preemption between the
            // two costs an extra barrier on the next dispatch; the other order
            // would claim a synchronisation that had not happened.
            synchronize_instruction_stream();
            cell.set(epoch);
            ISB_ISSUED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
    }
    #[cfg(target_arch = "x86_64")]
    {
        let _ = epoch;
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn platform_alloc(size: usize) -> Option<*mut u8> {
    use std::ptr;

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const PAGE_READWRITE: u32 = 0x04;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(
            lpAddress: *mut u8,
            dwSize: usize,
            flAllocationType: u32,
            flProtect: u32,
        ) -> *mut u8;
    }

    // Allocate as RW only — W^X enforcement.
    // SAFETY: a null address asks the OS to choose, so the new region cannot
    // alias an existing one; every other argument is a plain value, and a
    // failure is reported as a null return, handled below.
    let p = unsafe {
        VirtualAlloc(
            ptr::null_mut(),
            size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

#[cfg(target_os = "windows")]
fn platform_free(ptr: *mut u8, _size: usize) {
    const MEM_RELEASE: u32 = 0x8000;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualFree(lpAddress: *mut u8, dwSize: usize, dwFreeType: u32) -> i32;
    }

    // SAFETY: `ptr` is the base `VirtualAlloc` returned for this region (the
    // `free_executable` contract), and `MEM_RELEASE` with size 0 releases
    // exactly that reservation. The caller owns no reference into it after.
    unsafe {
        VirtualFree(ptr, 0, MEM_RELEASE);
    }
}

#[cfg(target_os = "windows")]
fn platform_make_executable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    const PAGE_EXECUTE_READ: u32 = 0x20;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualProtect(
            lpAddress: *mut u8,
            dwSize: usize,
            flNewProtect: u32,
            lpflOldProtect: *mut u32,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    // Non-x86 Windows (aarch64 / arm64ec) has split, non-coherent I-cache and
    // D-cache exactly like Linux aarch64. `VirtualProtect` alone does NOT
    // invalidate the I-cache, so without this the CPU can fetch stale bytes for
    // a freshly written (or freshly re-patched) JIT body. Done BEFORE the
    // RW→RX flip for the same reason as the Unix path: while the page is still
    // writable no instruction fetch can race the flush.
    //
    // x86-64/x86 Windows needs nothing (coherent caches, Intel SDM Vol.3 §11.6),
    // and `FlushInstructionCache` there would be a pure syscall cost on the JIT
    // hot path — so the *call* is arch-gated even though the helper is compiled
    // (and therefore type- and link-checked) on every Windows host.
    if !cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
        flush_icache_range_windows(ptr, size);
    }

    let mut old_protect: u32 = 0;
    // SAFETY: `ptr..ptr+size` lies inside a region `platform_alloc` committed,
    // and `old_protect` is a live local the call writes one `u32` into.
    let ret = unsafe { VirtualProtect(ptr, size, PAGE_EXECUTE_READ, &mut old_protect) };
    if ret == 0 {
        // VirtualProtect returns 0 on failure; surface GetLastError() so the
        // diagnostic carries the actual OS error code rather than the useless
        // `0` return value.
        // SAFETY: `GetLastError` takes no arguments and reads thread-local state.
        // Cast: a Win32 error code is a `DWORD`; `ProtectFailed` carries `i32`.
        let err = unsafe { GetLastError() } as i32;
        Err(JitError::ProtectFailed(err))
    } else {
        Ok(())
    }
}

/// Invalidate the CPU instruction cache over `[ptr, ptr+size)` on Windows.
///
/// `FlushInstructionCache(GetCurrentProcess(), base, len)` is the documented
/// Windows API for "I have just written instructions through the data path";
/// on ARM64 it issues the `DC CVAU` / `DSB ISH` / `IC IVAU` / `DSB ISH` / `ISB`
/// sequence the ARM ARM requires (B2.4.4, *Concurrent modification and
/// execution of instructions*). It is a documented no-op on x86/x86-64, whose
/// caches are coherent, which is why the call site above skips it there.
///
/// The function itself is compiled on every Windows target so a typo or a
/// signature mistake is caught by an ordinary x86-64 Windows build, not only by
/// a Windows-on-ARM cross-build nobody runs. Exercised on any Windows host by
/// `tests::windows_icache_flush_is_callable`.
#[cfg(target_os = "windows")]
fn flush_icache_range_windows(ptr: *mut u8, size: usize) {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn FlushInstructionCache(
            hProcess: *mut core::ffi::c_void,
            lpBaseAddress: *const core::ffi::c_void,
            dwSize: usize,
        ) -> i32;
    }
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle constant that is
    // always valid for the calling process, and `FlushInstructionCache` only
    // uses `ptr`/`size` as an address range to invalidate; it does not
    // dereference them, so any range (including an empty one) is sound.
    unsafe {
        // Returns BOOL; a failure here cannot be recovered from (we are about
        // to publish this code either way), and the only documented failure
        // mode is an invalid process handle, which `GetCurrentProcess` — a
        // pseudo-handle constant — cannot produce. Deliberately ignored.
        let _ = FlushInstructionCache(GetCurrentProcess(), ptr as *const core::ffi::c_void, size);
    }
}

#[cfg(target_os = "windows")]
fn platform_make_writable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    const PAGE_READWRITE: u32 = 0x04;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualProtect(
            lpAddress: *mut u8,
            dwSize: usize,
            flNewProtect: u32,
            lpflOldProtect: *mut u32,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    let mut old_protect: u32 = 0;
    // SAFETY: as in `platform_make_executable`: a committed region this module
    // allocated, and a live local for the old protection.
    let ret = unsafe { VirtualProtect(ptr, size, PAGE_READWRITE, &mut old_protect) };
    if ret == 0 {
        // VirtualProtect returns 0 on failure; surface GetLastError() so the
        // diagnostic carries the actual OS error code rather than the useless
        // `0` return value.
        // SAFETY: `GetLastError` takes no arguments and reads thread-local state.
        // Cast: a Win32 error code is a `DWORD`; `ProtectFailed` carries `i32`.
        let err = unsafe { GetLastError() } as i32;
        Err(JitError::ProtectFailed(err))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// macOS ARM64 (Apple Silicon) — per-thread W^X on MAP_JIT pages
// ---------------------------------------------------------------------------
//
// The previous shape mapped `MAP_JIT` pages RW and flipped them to RX with
// `mprotect`. The hardened runtime refuses that flip for a page that was ever
// writable, so `finalize` aborted the process on every compile of a hardened
// build, and nothing here ever called the toggle the platform actually
// provides. The supported protocol is: map once as RWX with `MAP_JIT`, then
// let `pthread_jit_write_protect_np` decide, per thread, whether those pages
// are writable (0) or executable (1). `JitWriteScope` holds the thread in write
// mode around each copy; outside a scope it is in execute mode, which is also
// the state a new thread starts in.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
extern "C" {
    fn pthread_jit_write_protect_np(enabled: i32);
    fn sys_icache_invalidate(start: *mut core::ffi::c_void, size: usize);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
thread_local! {
    /// How many [`JitWriteScope`]s this thread holds. Only the outermost one
    /// toggles, so a nested write cannot re-protect pages an outer write is
    /// still copying into.
    static JIT_WRITE_DEPTH: std::cell::Cell<usize> = std::cell::Cell::new(0);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_jit_write_begin() {
    JIT_WRITE_DEPTH.with(|depth| {
        let d = depth.get();
        if d == 0 {
            // SAFETY: takes a plain int and changes only this thread's view of
            // `MAP_JIT` pages. The matching re-protect is in
            // `platform_jit_write_end`, reached from `JitWriteScope::drop`.
            unsafe { pthread_jit_write_protect_np(0) };
        }
        depth.set(d + 1);
    });
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_jit_write_end() {
    JIT_WRITE_DEPTH.with(|depth| {
        let d = depth.get().saturating_sub(1);
        depth.set(d);
        if d == 0 {
            // SAFETY: as in `platform_jit_write_begin`; 1 restores execute mode.
            unsafe { pthread_jit_write_protect_np(1) };
        }
    });
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_alloc(size: usize) -> Option<*mut u8> {
    use std::ptr;

    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const PROT_EXEC: i32 = 4;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_JIT: i32 = 0x0800;
    const MAP_FAILED: *mut u8 = !0 as *mut u8;

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    }

    // RWX with MAP_JIT: the per-thread toggle, not the page protection, is
    // what enforces W^X on these pages.
    // SAFETY: a null hint without `MAP_FIXED` lets the kernel choose, so the
    // mapping cannot alias an existing one; every argument is a plain value and
    // a failure comes back as `MAP_FAILED`, handled below.
    let p = unsafe {
        mmap(
            ptr::null_mut(),
            size,
            PROT_READ | PROT_WRITE | PROT_EXEC,
            MAP_PRIVATE | map_anonymous(UnixMmapAbi::Bsd) | MAP_JIT,
            -1,
            0,
        )
    };
    if p == MAP_FAILED {
        None
    } else {
        Some(p)
    }
}

/// Plain RW anonymous memory for the code-adjacent data cells.
///
/// NOT `MAP_JIT`. Those cells are written by the RUNTIME (the safepoint flag is
/// set by whichever thread requests a stop), and a `MAP_JIT` page is writable
/// only by a thread that is currently in write mode -- every other thread would
/// fault on the store.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_alloc_data(size: usize) -> Option<*mut u8> {
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_FAILED: *mut u8 = !0 as *mut u8;

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    }

    // SAFETY: as in `platform_alloc` -- a kernel-placed anonymous mapping.
    let p = unsafe {
        mmap(
            std::ptr::null_mut(),
            size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | map_anonymous(UnixMmapAbi::Bsd),
            -1,
            0,
        )
    };
    if p == MAP_FAILED {
        None
    } else {
        Some(p)
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_free(ptr: *mut u8, size: usize) {
    extern "C" {
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    // SAFETY: `ptr`/`size` describe a mapping `platform_alloc` returned (the
    // `free_executable` contract), and nothing references it afterwards.
    unsafe {
        munmap(ptr, size);
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_make_executable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    // Flush the instruction cache -- required on ARM64, where the data and
    // instruction caches are not coherent. There is no permission change to
    // make: the region is already executable for every thread not currently
    // inside a `JitWriteScope`.
    // SAFETY: `sys_icache_invalidate` treats `ptr..ptr+size` as an address
    // range to discard cached decodes for; it does not dereference the memory.
    unsafe { sys_icache_invalidate(ptr.cast(), size) };
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_make_writable(_ptr: *mut u8, _size: usize) -> Result<(), JitError> {
    // Nothing to do at the page level: a write is permitted exactly while the
    // writing thread holds a `JitWriteScope`, which `ExecutableBuffer`'s write
    // paths take for themselves. Refusing here would break the patch protocol
    // for no gain.
    Ok(())
}

// ---------------------------------------------------------------------------
// Placing JIT code within RIP reach of the VM's global cells (Unix)
// ---------------------------------------------------------------------------

/// `CRATONVM_JIT_CODE_NEAR_GLOBALS=1` — ask `mmap` to place each JIT code
/// buffer within ±2GB of the VM's global guard cells, so the RIP-relative
/// encodings of the safepoint poll and the layout-epoch guard can actually be
/// used. **Default OFF**, Unix only; Windows already lands in reach without
/// help and is untouched.
///
/// # Why this exists: the short encodings were unreachable on System V, always
///
/// `internal/performance/c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md`
/// moved `LAYOUT_REPLACE_EPOCH` off the executable image and onto the VM heap
/// so that `CMP dword [rip+disp32], imm32` — one instruction and ten bytes
/// against three and nineteen — would come into range. On Windows it did, and
/// the four-site loop got 14.5% faster.
///
/// On Linux it did not, and not by bad luck. Measured on a Ubuntu 24.04 x86-64
/// host, one process:
///
/// ```text
/// LAYOUT_REPLACE_EPOCH (mimalloc heap)   0x2001E8103F0
/// stw_requested_flag   (mimalloc heap)   0x2000CD6E2C0     295 MB away
/// optimizing tier's code buffer (mmap)   0x7A53DBCB8000    ~130 TB away
/// ```
///
/// The two heap cells are comfortably within reach of each other and neither
/// is within reach of the code. `mmap(NULL, …)` places anonymous mappings in
/// the kernel's own region, high in the address space; mimalloc reserves its
/// arenas near 2TB. Nothing moves those two towards each other, so on this
/// platform **every** guard and **every** back-edge poll took the long form,
/// on every compile, exactly as they did before the counter was moved. The
/// fallback path was not a safety net, it was the whole implementation.
///
/// # What this does
///
/// Nothing that can produce a wrong address, because it only supplies a HINT.
/// `mmap` without `MAP_FIXED` treats its first argument as a suggestion: it
/// never unmaps or overlaps an existing mapping, so this cannot land JIT code
/// on top of the heap, on top of a reserved arena, or anywhere the GC reads.
/// The kernel is free to ignore the hint entirely, and when the returned
/// mapping is out of reach it is released and the next hint tried.
///
/// The window is probed on BOTH sides of the anchor with doubling
/// offsets (`LADDER`), because the room turned out to be under the allocator's
/// arena rather than above it — and once a placement succeeds the next
/// allocation is asked for beside it, so the ladder is walked once and not per
/// compile. A whole ladder that misses retires the strategy permanently: an
/// address space with no room near the anchor pays eight wasted `mmap`/`munmap`
/// pairs once and none afterwards.
///
/// "A whole ladder that misses" is a narrower condition than it sounds, and
/// getting it wrong is what `r10-readers-near-globals-place-retires-on-a-stale-cursor-FIXED-20260922.md`
/// describes: it means every rung that exists at this anchor was handed to
/// `mmap` and every mapping came back out of reach. A step that had no address
/// to offer for the SIZE of one particular buffer is not that, because a smaller
/// buffer would be offered one — so it costs that allocation and not the
/// strategy. [`near_globals::walk`] is where the two are separated and
/// [`near_globals::Hint`] is why they can be.
///
/// # Why the anchor is the epoch counter and not the safepoint flag
///
/// The flag lives in the VM's `gc_barrier` allocation, which the JIT crate
/// cannot name without a VM dependency; the counter is in `cratonvm_types`,
/// which it already depends on. They come from the same allocator and, as
/// measured above, sit within a few hundred megabytes of each other — so
/// anchoring on either brings both into reach, and the range check in
/// `ir_lower::rip_disp32` is what actually decides per site.
#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
mod near_globals {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Steps of the walk [`place`] takes before it gives up on ONE allocation.
    ///
    /// `LADDER.len() + 1`, and the `+ 1` is not slack: when a packing cursor is
    /// held it occupies step 0, so the eight ladder rungs occupy steps 1..=8 and
    /// the loop needs nine iterations to reach rung 7. With no cursor the rungs
    /// occupy steps 0..=7 and step 8 reports [`Hint::Exhausted`]. Either way the
    /// whole window is probed rather than the first few megabytes of it.
    ///
    /// Reaching this bound is NOT by itself a reason to retire the strategy —
    /// see [`walk`], which retires only when every rung was actually mapped and
    /// every mapping landed out of reach.
    const NEAR_GIVE_UP_AFTER: usize = LADDER.len() + 1;

    /// How far from the anchor a buffer may sit and still be usable. Half a
    /// gigabyte inside the architectural ±2GB, because the displacement is
    /// measured from the END of an instruction somewhere inside the buffer and
    /// the anchor is one cell of a heap that keeps growing around it.
    const NEAR_WINDOW: usize = 1_500_000_000;

    /// Offsets from the anchor to suggest, **below it first and doubling**.
    ///
    /// # Why a ladder and not a bump, which is what this was first
    ///
    /// A cursor starting just above the anchor and walking up in
    /// allocation-sized steps found nothing, ever, and the address space says
    /// why. Measured on the Ubuntu host, `/proc/<pid>/maps`:
    ///
    /// ```text
    /// 20000000000-20040000000 rw-p [anon:mimalloc]
    /// ```
    ///
    /// **One 16GB reservation**, with the anchor 511MB inside it. Everything
    /// within 1.5GB *above* the anchor is inside that mapping, so every hint
    /// was relocated and the strategy retired itself having probed 16MB of a
    /// 3GB window. The free space is 511MB *below* the anchor, under the
    /// arena's base — which a one-directional walk in 2MB steps would have
    /// needed 255 probes to reach.
    ///
    /// So: both directions, doubling, below first because an arena is
    /// reserved upward from its base and the room is underneath it.
    const LADDER: [(bool, usize); 8] = [
        (false, 64 << 20),
        (true, 64 << 20),
        (false, 256 << 20),
        (true, 256 << 20),
        (false, 512 << 20),
        (true, 512 << 20),
        (false, 1 << 30),
        (true, 1 << 30),
    ];

    /// 2MB, so hints are transparent-hugepage aligned and consecutive buffers
    /// do not straddle.
    const HINT_ALIGN: usize = 2 << 20;

    static ANCHOR: AtomicUsize = AtomicUsize::new(0);
    /// Where the last successful placement ended, so the next buffer is asked
    /// for beside it instead of re-walking the ladder. Zero means "no
    /// placement has succeeded yet".
    static CURSOR: AtomicUsize = AtomicUsize::new(0);
    static RETIRED: AtomicBool = AtomicBool::new(false);

    /// ENGAGEMENT: buffers placed in reach, and buffers that fell back.
    ///
    /// Read as a PAIR and never alone. `in_reach=0` alone cannot separate "the
    /// flag is off" from "the flag is on and this host has no room near the
    /// anchor"; `fell_back` beside it does, and `RETIRED` says whether the
    /// ladder has stopped being walked at all.
    ///
    /// `FELL_BACK` counts every `None` that [`place`] returns while the strategy
    /// is ENABLED — the post-retirement early return, a missing anchor, a failed
    /// hinted `mmap`, and the ladder running out of rungs. It deliberately does
    /// NOT count the `!enabled()` return: that is not a fallback, and counting
    /// it would put a relaxed atomic on every JIT buffer allocation in the
    /// default configuration.
    static IN_REACH: AtomicUsize = AtomicUsize::new(0);
    static FELL_BACK: AtomicUsize = AtomicUsize::new(0);

    fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            matches!(
                cratonvm_types::flags::runtime_var("CRATONVM_JIT_CODE_NEAR_GLOBALS").as_deref(),
                Ok("1") | Ok("true") | Ok("on") | Ok("yes")
            )
        })
    }

    /// The cell JIT code most wants to reach, forced into existence now so its
    /// address is stable before the first buffer is placed.
    fn anchor() -> usize {
        let a = ANCHOR.load(Ordering::Relaxed);
        if a != 0 {
            return a;
        }
        // Cast: a `*const u32` used only as a number to compute a hint from.
        let addr = cratonvm_types::field_layout::layout_replace_epoch_guard().0 as usize;
        ANCHOR.store(addr, Ordering::Relaxed);
        addr
    }

    pub(super) fn in_reach(p: usize, size: usize, anchor: usize) -> bool {
        let lo = p;
        let hi = p.saturating_add(size);
        lo.abs_diff(anchor) < NEAR_WINDOW && hi.abs_diff(anchor) < NEAR_WINDOW
    }

    /// What one step of the walk offers, **and why it offers nothing when it
    /// offers nothing**.
    ///
    /// # Why this is not `Option<usize>`, which is what it was
    ///
    /// `nth_hint` used to return `Option<usize>` and [`place`] used to `break`
    /// on `None`, falling straight through to the statement that sets
    /// [`RETIRED`]. Three unrelated conditions produced that one `None`:
    ///
    /// 1. the ladder had no rung numbered `attempt` — the ONLY case the
    ///    retirement below the loop is written for ("the whole ladder missed");
    /// 2. a held [`CURSOR`] could not hold THIS allocation's `size` inside the
    ///    window, at step 0, before a single rung had been probed;
    /// 3. a rung's address could not hold this `size` inside the window either.
    ///
    /// Cases 2 and 3 retired the near-globals strategy for the life of the
    /// process on the strength of having probed zero or a few rungs, and every
    /// later `platform_alloc` — including a 64-byte lambda adapter that would
    /// have packed beside the cursor without trouble — returned from [`place`]
    /// immediately. Case 2 is reachable by construction and gets MORE likely the
    /// better the strategy is working: `CURSOR` is only ever stored when
    /// `in_reach(next, size, anchor)` held for the size of the allocation that
    /// stored it, it walks monotonically toward the window edge, and JIT buffer
    /// sizes vary by orders of magnitude across the `ExecutableBuffer` call
    /// sites (`code.len().max(64)` in `lambda_adapter`, a whole-body estimate in
    /// the optimizing tier). A larger `size` re-checked against the same cursor
    /// is strictly harder to satisfy, because `in_reach` measures `p + size`.
    ///
    /// Filed as
    /// `docs/internal/fixed-bugs/r10-readers-near-globals-place-retires-on-a-stale-cursor-FIXED-20260922.md`.
    /// An enum is what stops the two "nothing here" answers from being the same
    /// value again, which is the form of the bug rather than its instance.
    ///
    /// The distinction the retirement decision needs is **permanent versus
    /// size-dependent**, not merely "no address":
    ///
    /// * [`Hint::TooBigHere`] is size-dependent. A SMALLER request offered the
    ///   same step would get an address, so this step says nothing about
    ///   whether the address space has room near the anchor and must not
    ///   contribute to retirement.
    /// * [`Hint::NoSuchAddress`] is permanent for every size — the offset
    ///   under/overflows the address space at this anchor, or a held cursor's
    ///   BASE has itself left the window. Skipping it costs nothing and it does
    ///   not stand in the way of a retirement the rest of the ladder earns.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Hint {
        /// Suggest this address to `mmap`.
        At(usize),
        /// There is an address at this step, and `size` bytes starting there
        /// would leave the window. Size-dependent: skip, do NOT retire.
        TooBigHere,
        /// This step has no address at this anchor for ANY size. Skip; it is
        /// permanently absent, so it neither earns nor blocks retirement.
        NoSuchAddress,
        /// There is no step numbered `attempt`: the ladder is walked out.
        Exhausted,
    }

    impl Hint {
        /// The address when there is one.
        ///
        /// Present so a reader (and the tests below) can ask the pre-enum
        /// question — "is there a hint here?" — without the enum's distinction
        /// leaking into every call site that does not need it. [`walk`] matches
        /// on the enum itself, because the distinction is the whole point there.
        pub(super) fn addr(self) -> Option<usize> {
            match self {
                Hint::At(a) => Some(a),
                Hint::TooBigHere | Hint::NoSuchAddress | Hint::Exhausted => None,
            }
        }
    }

    /// The `attempt`-th address to suggest for `size` bytes.
    ///
    /// Attempt 0 is `cursor` when one is held (a non-zero `cursor` means a
    /// previous placement succeeded and the next buffer should pack beside
    /// it); after that it is the ladder, in order. `cursor` is a parameter and
    /// not a read of the static so that one call to [`place`] sees one value
    /// of it — otherwise clearing a stale cursor mid-walk would renumber the
    /// rungs underneath the loop and skip one.
    pub(super) fn nth_hint(attempt: usize, cursor: usize, size: usize, anchor: usize) -> Hint {
        let rung = if cursor != 0 {
            if attempt == 0 {
                // `in_reach(cursor, 0, anchor)` is the BASE-only question: with
                // `size == 0`, `hi == lo`, so both halves of that predicate
                // collapse to `cursor.abs_diff(anchor) < NEAR_WINDOW`. A cursor
                // whose base has left the window refuses EVERY size, and it will
                // keep refusing: the only things that ever change `CURSOR` are a
                // successful placement, which overwrites it, and a clear. So it
                // is permanently absent rather than merely too small for this
                // request — and [`walk`] clears it when it sees this, which is
                // what makes "permanently" a property of the code and not of the
                // arithmetic.
                return if !in_reach(cursor, 0, anchor) {
                    Hint::NoSuchAddress
                } else if in_reach(cursor, size, anchor) {
                    Hint::At(cursor)
                } else {
                    Hint::TooBigHere
                };
            }
            attempt - 1
        } else {
            attempt
        };
        let Some(&(up, off)) = LADDER.get(rung) else {
            return Hint::Exhausted;
        };
        let based = if up {
            anchor.checked_add(off)
        } else {
            anchor.checked_sub(off)
        };
        let Some(addr) = based else {
            // The offset does not exist at this anchor — an anchor lower than
            // the rung's offset, or an add that would leave the address space.
            // Independent of `size`, so `NoSuchAddress` and not `TooBigHere`.
            return Hint::NoSuchAddress;
        };
        // Rounds DOWN, which shortens an upward rung's reach and lengthens a
        // downward one's; both stay inside the window, which `in_reach` is what
        // actually confirms.
        let addr = addr & !(HINT_ALIGN - 1);
        if in_reach(addr, size, anchor) {
            Hint::At(addr)
        } else {
            Hint::TooBigHere
        }
    }

    /// What [`CURSOR`] should hold once a walk has finished.
    ///
    /// Three cases and not an `Option<usize>`, because "store 0" and "leave it
    /// alone" are different instructions to [`apply_cursor`] and "clear it if it
    /// is still the value I read" is a third. Keeping them apart in the return
    /// value is what lets [`walk`] be a pure function of its arguments — see
    /// [`Walk`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum CursorUpdate {
        /// Leave [`CURSOR`] as it is.
        Keep,
        /// Store this value unconditionally. Only a walk that PLACED something
        /// says this, and its value is derived from a mapping that exists right
        /// now, so it is newer than anything another thread could have seeded.
        Set(usize),
        /// Clear it, but only if it still holds the value this walk read.
        ClearIfUnchanged,
    }

    /// The outcome of ONE walk of the ladder.
    ///
    /// # Why this exists as a value instead of [`walk`] touching the statics
    ///
    /// The retirement DECISION is the thing that was wrong, and it was
    /// untestable: [`RETIRED`] is a process-global `AtomicBool` with no way to
    /// reset it, [`enabled`] latches an environment variable in a `OnceLock`, and
    /// [`place`]'s only observable result was `Option<*mut u8>` — so a test could
    /// not tell "this allocation found nothing" from "the strategy is now off for
    /// the process", which is precisely the distinction the bug erased. Returning
    /// the decision as data makes it assertable without a flag, without an
    /// `mmap`, and without a way to un-retire anything.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct Walk {
        /// The mapping to hand back, or `None` to fall back to an unhinted map.
        pub(super) placed: Option<*mut u8>,
        /// What to do to [`CURSOR`].
        pub(super) cursor: CursorUpdate,
        /// Whether the strategy should retire for the rest of the process.
        pub(super) retire: bool,
        /// Steps whose hint was actually handed to `map`, i.e. syscalls spent.
        ///
        /// This is what the `RETIRED` diagnostic prints. It used to print the
        /// constant `NEAR_GIVE_UP_AFTER` unconditionally, so the one line that
        /// could have diagnosed a premature retirement asserted a full
        /// eight-rung walk on a path that had probed none.
        pub(super) probed: usize,
        /// The step index at which the placement happened, for the `placed`
        /// line. Meaningless when `placed` is `None`.
        pub(super) attempt: usize,
        /// `map` itself failed, so the walk stopped early. Not a retirement
        /// reason: an unhinted retry may still succeed.
        pub(super) map_failed: bool,
    }

    /// Walk the hint ladder once for `size` bytes, using `map`/`unmap`.
    ///
    /// Pure with respect to this module's statics: everything it decides comes
    /// back in the [`Walk`], and [`place`] is the only thing that writes
    /// [`CURSOR`], [`RETIRED`] or the counters. See [`Walk`] for why.
    ///
    /// # The retirement rule, which is the whole point of this function
    ///
    /// Retire only when **at least one ladder rung was mapped and every rung
    /// that exists at this anchor was mapped and landed out of reach.** Spelled
    /// out as the two counters below:
    ///
    /// * `rungs_probed > 0` — something was actually tried. A walk that handed
    ///   no address to `mmap` has learned nothing about the address space and
    ///   must not switch the strategy off; the previous code retired on exactly
    ///   that, after a `break` at step 0.
    /// * `rungs_too_big == 0` — no rung was skipped for a reason that a smaller
    ///   request would not hit. If any was, this walk is not evidence about the
    ///   address space, it is evidence about this allocation's size, and the next
    ///   smaller buffer gets a full walk.
    ///
    /// A [`Hint::NoSuchAddress`] rung is deliberately absent from both: it can
    /// never be probed at this anchor for any size, so requiring it to be probed
    /// would mean a process whose anchor sits below the deepest rung never
    /// retires and pays eight `mmap`/`munmap` pairs per compile forever — which
    /// is the cost retirement exists to avoid.
    ///
    /// The cursor step is not a rung and contributes to neither counter. It is
    /// one address the caller happens to prefer, not part of the ladder whose
    /// exhaustion "the whole ladder missed" is about.
    pub(super) fn walk(
        size: usize,
        cursor: usize,
        anchor: usize,
        map: impl Fn(*mut u8) -> Option<*mut u8>,
        unmap: impl Fn(*mut u8),
    ) -> Walk {
        let mut probed = 0usize;
        let mut rungs_probed = 0usize;
        let mut rungs_too_big = 0usize;
        let mut cursor_update = CursorUpdate::Keep;
        for attempt in 0..NEAR_GIVE_UP_AFTER {
            // Step 0 is the held cursor and is NOT a ladder rung; with no cursor
            // held every step is a rung. Mirrors `nth_hint`'s own numbering.
            let on_ladder = cursor == 0 || attempt > 0;
            match nth_hint(attempt, cursor, size, anchor) {
                Hint::Exhausted => break,
                Hint::NoSuchAddress => {
                    if !on_ladder {
                        // The held cursor's BASE has left the window. It is out
                        // for every size from here on, so clear it: this is the
                        // "a cursor that has walked out of the window is cleared
                        // rather than kept, so the next allocation re-enters at
                        // the ladder" behaviour that the comment on the success
                        // path has always claimed and that no path performed.
                        cursor_update = CursorUpdate::ClearIfUnchanged;
                    }
                    continue;
                }
                Hint::TooBigHere => {
                    if on_ladder {
                        rungs_too_big += 1;
                    }
                    // A held cursor that cannot hold THIS size is kept: its base
                    // is still inside the window, so a smaller buffer can still
                    // pack beside it, and throwing it away would cost that
                    // buffer a ladder walk for nothing.
                    continue;
                }
                Hint::At(hint) => {
                    probed += 1;
                    if on_ladder {
                        rungs_probed += 1;
                    }
                    // Cast: a hint address, which `mmap` may ignore.
                    let Some(p) = map(hint as *mut u8) else {
                        return Walk {
                            placed: None,
                            cursor: cursor_update,
                            retire: false,
                            probed,
                            attempt,
                            map_failed: true,
                        };
                    };
                    if in_reach(p as usize, size, anchor) {
                        let end = (p as usize).saturating_add(size);
                        // `saturating_add` on the round-up too: `end` is already
                        // saturated, and `end + HINT_ALIGN - 1` on a saturated
                        // `end` overflows. Unreachable with a real mapping, and
                        // a debug-build panic in an allocation path is not a
                        // thing to leave standing on the strength of that.
                        let next = end.saturating_add(HINT_ALIGN - 1) & !(HINT_ALIGN - 1);
                        return Walk {
                            placed: Some(p),
                            // A cursor that has already walked out of the window
                            // is stored as 0 rather than kept, so the next
                            // allocation re-enters at the ladder instead of
                            // proposing an address the check would refuse.
                            cursor: CursorUpdate::Set(if in_reach(next, size, anchor) {
                                next
                            } else {
                                0
                            }),
                            retire: false,
                            probed,
                            attempt,
                            map_failed: false,
                        };
                    }
                    unmap(p);
                    if !on_ladder {
                        // The packing address is occupied by something else.
                        // Keeping it would spend one wasted `mmap`/`munmap` pair
                        // on every future allocation; the ladder re-seeds it on
                        // its next hit.
                        cursor_update = CursorUpdate::ClearIfUnchanged;
                    }
                }
            }
        }
        Walk {
            placed: None,
            cursor: cursor_update,
            retire: rungs_probed > 0 && rungs_too_big == 0,
            probed,
            attempt: NEAR_GIVE_UP_AFTER,
            map_failed: false,
        }
    }

    /// Try to place `size` bytes near the anchor. `None` means the caller
    /// should fall back to an unhinted mapping.
    ///
    /// `map` is `platform_alloc`'s raw `mmap` and `unmap` its `munmap`, passed
    /// in so this module holds no `extern` declarations of its own and cannot
    /// drift from the flags the real allocator uses.
    ///
    /// The walk itself is [`walk`]; this function is the part that reads and
    /// writes the module's statics and the census, and it is deliberately the
    /// ONLY part that does.
    pub(super) fn place(
        size: usize,
        map: impl Fn(*mut u8) -> Option<*mut u8>,
        unmap: impl Fn(*mut u8),
    ) -> Option<*mut u8> {
        // The flag being off is NOT a fallback and must not be counted: with
        // `CRATONVM_JIT_CODE_NEAR_GLOBALS` unset this strategy was never asked
        // for anything, and counting here would also put one relaxed atomic on
        // every JIT buffer allocation in the default configuration. This return
        // is before any counter for both reasons.
        if !enabled() {
            return None;
        }
        // Every OTHER `None` out of this function IS a buffer that fell back to
        // an unhinted mapping, and `FELL_BACK` now counts all of them.
        //
        // It used to be incremented in exactly one place — the ladder-exhausted
        // path at the bottom, on the same line that sets `RETIRED` — and because
        // `place` returns here as soon as `RETIRED` is set, that path runs AT
        // MOST ONCE per process. So `FELL_BACK` could only ever read 0 or 1
        // (2 under a benign race between two concurrent walkers), while its own
        // doc called it "buffers that fell back" and `stats()` published it as
        // the second element of `(placed in reach, fell back, retired)`. On a
        // host with no room near the anchor, thousands of buffers fell back and
        // the census read `in_reach=0 fell_back=1` — which is the reading a
        // person uses to decide whether the flag engaged, and it was off by
        // three orders of magnitude in the direction that says "nothing
        // happened".
        if RETIRED.load(Ordering::Relaxed) {
            FELL_BACK.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let anchor = anchor();
        if anchor == 0 {
            // Enabled, asked, and could not even find the anchor to measure
            // against. That is a fallback like any other.
            FELL_BACK.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // Read once: see `nth_hint`'s contract.
        let cursor = CURSOR.load(Ordering::Relaxed);
        let w = walk(size, cursor, anchor, map, unmap);
        if let Some(p) = w.placed {
            // Counted BEFORE `report`, which is not cosmetic. `report` calls
            // `stats()` and prints `in_reach=`, so with the `fetch_add` after
            // the `report` (where it used to be) the line printed for the FIRST
            // successful placement said `in_reach=0` — a placement announcing
            // that nothing had been placed. The retirement path below already
            // increments before it reports, so the two halves of one diagnostic
            // used opposite conventions, and the half that disagreed was the one
            // a reader consults to decide whether the strategy engaged at all.
            IN_REACH.fetch_add(1, Ordering::Relaxed);
            report("placed", anchor, p as usize, size, w.attempt);
            apply_cursor(w.cursor, cursor);
            return Some(p);
        }
        apply_cursor(w.cursor, cursor);
        FELL_BACK.fetch_add(1, Ordering::Relaxed);
        if w.retire {
            // The whole ladder missed: every rung that exists at this anchor was
            // mapped and the kernel put every one of them out of reach. Nothing
            // in this address space is going to change that within the process,
            // and re-walking it per compile is eight wasted `mmap`/`munmap`
            // pairs each time.
            //
            // Set BEFORE `report`, so the `retired=` field of the line that
            // announces the retirement reads `true` rather than describing the
            // state one statement ago.
            RETIRED.store(true, Ordering::Relaxed);
            report("RETIRED", anchor, 0, size, w.probed);
        } else if w.map_failed {
            // A hinted `mmap` failed outright. An unhinted retry may still
            // work, so this is not a reason to retire — and it was previously
            // indistinguishable in the log from the retirement, because the
            // only two lines this module could print were `placed` and
            // `RETIRED`.
            report("map-failed", anchor, 0, size, w.probed);
        } else {
            // This ALLOCATION found nothing and the STRATEGY is still armed:
            // some step of the walk had no address for a request this size, so
            // the walk cannot say the address space is out of room. A smaller
            // buffer will walk again. This outcome had no spelling at all
            // before — it was reported as `RETIRED`, and it retired.
            report("unplaced", anchor, 0, size, w.probed);
        }
        None
    }

    /// Apply a walk's [`CursorUpdate`] to [`CURSOR`].
    ///
    /// `read` is the value [`place`] loaded before the walk, which is what the
    /// `ClearIfUnchanged` compare-exchange compares against.
    fn apply_cursor(update: CursorUpdate, read: usize) {
        match update {
            CursorUpdate::Keep => {}
            CursorUpdate::Set(v) => CURSOR.store(v, Ordering::Relaxed),
            CursorUpdate::ClearIfUnchanged => {
                // Not a plain store: a concurrent walk may have placed a buffer
                // and seeded a fresh cursor since `read` was taken, and clearing
                // THAT one would cost the next allocation its packing locality
                // for a staleness it does not have.
                let _ = CURSOR.compare_exchange(read, 0, Ordering::Relaxed, Ordering::Relaxed);
            }
        }
    }

    /// `CRATONVM_DBG_CODE_NEAR_GLOBALS=1` — one line per placement DECISION,
    /// which is the only way a run can be shown to have engaged this strategy
    /// rather than merely to have set its flag.
    ///
    /// # Why this is not optional bookkeeping
    ///
    /// [`stats`] has said since it was written that it exists "for the
    /// diagnostic line", and there was no diagnostic line — nothing in the
    /// process read it. That gap is not cosmetic on THIS flag: it is a hint to
    /// `mmap` and the kernel is free to ignore it, so "the flag was set" and
    /// "the code moved" are genuinely different facts, and on a host with no
    /// room near the anchor the second is false while the first is true. A
    /// differential run whose arms are indistinguishable from the outside
    /// proves nothing, and before this line the only way to tell them apart
    /// was to disassemble a method and count `81 3D` opcodes.
    ///
    /// `[near-globals] placed anchor=0x… buf=0x… size=… delta=…MB attempt=N
    /// in_reach=… fell_back=… retired=…`
    ///
    /// `delta` is what actually decides the encoding, so it is printed rather
    /// than left for the reader to subtract: a buffer inside the window is
    /// what makes `rip_disp32` hand out the short form.
    ///
    /// # The four outcomes, and why there used to be two
    ///
    /// * `placed` — a mapping landed in reach. `attempt` is the step index that
    ///   produced it: 0 is the packing cursor, and with a cursor held rung *r*
    ///   is step *r + 1*.
    /// * `RETIRED` — the whole ladder was walked, every rung that exists at this
    ///   anchor was mapped, and every mapping came back out of reach. The
    ///   strategy is now off for the rest of the process.
    /// * `unplaced` — this allocation found nothing and the strategy is STILL
    ///   ARMED, because some step had no address for a request this size.
    /// * `map-failed` — a hinted `mmap` failed outright.
    ///
    /// On the miss lines `attempt` is the number of hints actually handed to
    /// `mmap`, i.e. syscalls spent — NOT the loop bound.
    ///
    /// Both halves of that sentence were wrong before. Only `placed` and
    /// `RETIRED` existed, so the two "nothing was placed, keep going" cases
    /// printed the word `RETIRED` (and retired: see [`Hint`]); and the `RETIRED`
    /// line passed the constant `NEAR_GIVE_UP_AFTER` as its attempt count, so it
    /// asserted a full eight-rung walk unconditionally — including on the path
    /// that had probed zero rungs. The one line a person would have used to
    /// diagnose the premature retirement stated the thing that was not true, in
    /// the direction that made the retirement look earned.
    fn report(outcome: &str, anchor: usize, buf: usize, size: usize, attempt: usize) {
        if !cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_CODE_NEAR_GLOBALS") {
            return;
        }
        let (in_reach_n, fell_back_n, retired) = stats();
        // Megabytes, integer: the question this answers is "inside ±1.5GB or
        // not", and a byte count makes that harder to read, not easier.
        let delta_mb = if buf == 0 {
            0
        } else {
            buf.abs_diff(anchor) / (1 << 20)
        };
        eprintln!(
            "[near-globals] {outcome} anchor={anchor:#x} buf={buf:#x} size={size} \
             delta={delta_mb}MB attempt={attempt} in_reach={in_reach_n} \
             fell_back={fell_back_n} retired={retired}"
        );
    }

    /// The window [`in_reach`] enforces, so a test can state the architectural
    /// limit it must stay inside rather than restating the number.
    pub(super) const WINDOW: usize = NEAR_WINDOW;

    /// Whether this strategy owns placement in this process.
    ///
    /// Read by [`super::alloc_code_adjacent_cell`], which must NOT hand out a
    /// cell while this is on — see its doc. Not `stats().2`: a strategy that
    /// has retired still owned the decision, and the cells it declined were
    /// declined for the whole process.
    pub(super) fn engaged() -> bool {
        enabled()
    }

    /// `(placed in reach, fell back, retired)` — for the diagnostic line and
    /// for `jit/tests/r10_nearglobals_census_default_configuration.rs`.
    ///
    /// # The history of this sentence, because it drifted twice
    ///
    /// It first read "for the diagnostic line", and there was no diagnostic
    /// line — [`report`]'s doc records that and the line was written. Its second
    /// half then read "and for a test that would otherwise be unable to tell
    /// 'the flag works' from 'the flag is inert on this machine'", and there was
    /// no such test either; round 10 wave 6 replaced that clause with the plain
    /// statement that `near_globals_tests` imported `{in_reach, nth_hint,
    /// WINDOW}` and not `stats`, and filed the absence as item 1 of
    /// `docs/internal/fixed-bugs/r10-readers-platform-dead-public-surface-RESOLVED-20260922.md`
    /// — a `pub use` of a private module's accessor whose only purpose is to be
    /// called from outside, called from nowhere.
    ///
    /// That test now exists, as the integration binary named above, and it is
    /// the caller [`super::code_near_globals_stats`] was re-exported for. It
    /// asserts the invariant this census's own documentation rests on: that in
    /// the DEFAULT configuration the `!enabled()` return in [`place`] comes
    /// before every counter, so `(0, 0)` is what an unset flag reads and not
    /// something a reader has to take on trust. It is an integration binary
    /// rather than a unit test because these are process-global and monotone —
    /// a sibling in the same process could drive them up, and a must-be-zero
    /// assertion can only be written where nothing else runs.
    ///
    /// What it deliberately does NOT assert is "the flag works", which is the
    /// claim the removed clause promised. That needs a run with the flag set on
    /// a host with room near the anchor, and on a host without room the honest
    /// reading is `in_reach=0 fell_back=N`, which is indistinguishable from a
    /// bug by design. The retirement DECISION — the part that was wrong — is
    /// pinned instead by the `walk` tests in `near_globals_tests`, which need
    /// neither the flag nor the census.
    pub fn stats() -> (usize, usize, bool) {
        (
            IN_REACH.load(Ordering::Relaxed),
            FELL_BACK.load(Ordering::Relaxed),
            RETIRED.load(Ordering::Relaxed),
        )
    }
}

#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
pub use near_globals::stats as code_near_globals_stats;

/// Windows and macOS/ARM64 do not hint: the former already lands in reach and
/// the latter allocates `MAP_JIT` pages the kernel places on its own terms.
///
/// The constant `(0, 0, true)` is not "nothing happened": the `true` says
/// placement is not this target's business at all, which is a different reading
/// from a Unix host whose flag is unset (`(0, 0, false)`) and from one whose
/// ladder missed (`(0, N, true)`). `jit/tests/r10_nearglobals_census_default_configuration.rs`
/// asserts both shapes per platform, so this stub and the real accessor cannot
/// silently stop agreeing about which name a caller gets.
#[cfg(any(
    target_os = "windows",
    all(target_os = "macos", target_arch = "aarch64")
))]
pub fn code_near_globals_stats() -> (usize, usize, bool) {
    (0, 0, true)
}

// ---------------------------------------------------------------------------
// Unix (non-macOS-ARM64) — Linux, the BSDs, macOS x86-64, illumos
// ---------------------------------------------------------------------------

#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
fn platform_alloc(size: usize) -> Option<*mut u8> {
    use std::ptr;

    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_FAILED: *mut u8 = !0 as *mut u8;
    // Per OS, not Linux's value everywhere -- see `UnixMmapAbi`.
    let map_anon = map_anonymous(HOST_MMAP_ABI?);

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }

    // Allocate as RW only — W^X enforcement.
    //
    // `hint` is `mmap`'s first argument and nothing more: without `MAP_FIXED`
    // the kernel may place the mapping anywhere, and will never disturb an
    // existing one. A null hint is the historical behaviour exactly.
    let raw = |hint: *mut u8| -> Option<*mut u8> {
        // SAFETY: no `MAP_FIXED`, so `hint` is advisory and the kernel never
        // replaces an existing mapping; the arguments are plain values and a
        // failure comes back as `MAP_FAILED`, handled below.
        let p = unsafe {
            mmap(
                hint,
                size,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | map_anon,
                -1,
                0,
            )
        };
        if p == MAP_FAILED {
            None
        } else {
            Some(p)
        }
    };
    let release = |p: *mut u8| {
        // Straight `munmap`, never the poisoning path: this region was mapped
        // microseconds ago, held no code, and was never executable.
        // SAFETY: `p` is a mapping of exactly `size` bytes that `raw` returned
        // a moment ago and that nothing else has seen.
        unsafe {
            munmap(p, size);
        }
    };
    if let Some(p) = near_globals::place(size, &raw, &release) {
        return Some(p);
    }
    raw(ptr::null_mut())
}

#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
fn platform_free(ptr: *mut u8, size: usize) {
    extern "C" {
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    }
    // SAFETY: `ptr`/`size` describe a mapping `platform_alloc` returned (the
    // `free_executable` contract). Either call retires it: `munmap` releases
    // it, `mprotect(PROT_NONE)` leaves it mapped but inaccessible. Nothing may
    // reference the region afterwards, which is the caller's obligation.
    unsafe {
        if jit_poison_free_enabled() {
            // DIAG: keep the mapping, make it permanently inaccessible, never
            // recycle the address. A later jump into this retired body still
            // faults at the same moment it would have, but the region is still
            // in /proc/self/maps (as `---p`), which positively identifies the
            // fault as "jumped into retired JIT code" instead of leaving an
            // unmapped address with no provenance.
            const PROT_NONE: i32 = 0;
            // The return value is CHECKED, and the counter moves only on
            // success. It used to be `mprotect(ptr, size, PROT_NONE);` with the
            // result discarded and an unconditional `fetch_add` underneath,
            // which made this the one protect call in this file whose failure
            // was neither propagated (as `platform_make_executable` and
            // `platform_make_writable` do, via `last_os_error_code`) nor logged
            // (as `JitCodeArena::release` does, by stranding the block and
            // warning).
            //
            // Silently wrong in the dangerous direction: on failure the early
            // `return` means no `munmap` runs either, so the region is left
            // FULLY READABLE AND EXECUTABLE — while
            // `vm/src/jit/code_cache_lifecycle.rs`'s report told the reader
            // those exact bytes were "left mapped PROT_NONE and never
            // recycled". An operator using this mode to prove a
            // use-after-free jump would have been reading a count of bytes that
            // were, on that path, still live code.
            //
            // `mprotect` returns 0 on success and -1 on failure (setting
            // `errno`), so `== 0` is the success test; the module already
            // captures `errno` via `last_os_error_code` for exactly this kind of
            // call. On failure we fall through to `munmap` rather than
            // returning, because retiring the mapping is strictly safer than
            // leaving executable bytes behind, and the `warn!` says the
            // diagnostic did not get what it asked for.
            if mprotect(ptr, size, PROT_NONE) == 0 {
                POISONED_JIT_BYTES.fetch_add(size, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            tracing::warn!(
                size = size,
                errno = last_os_error_code(),
                "CRATONVM_JIT_POISON_FREE: mprotect(PROT_NONE) failed on a retired \
                 JIT code mapping; unmapping it instead. These bytes are NOT \
                 counted in POISONED_JIT_BYTES and this address CAN be reused, so \
                 a later jump into this retired body will not be identifiable as \
                 one. The poison diagnostic is not in force for this buffer."
            );
        }
        munmap(ptr, size);
    }
}

/// DIAG (2026-07-27): `CRATONVM_JIT_POISON_FREE=1` retires executable buffers
/// with `mprotect(PROT_NONE)` instead of `munmap`, so a use-after-free jump into
/// retired JIT code is directly observable (the range stays mapped and the
/// address is never reused). Leaks address space by construction.
///
/// **UNIX ONLY, EXCLUDING macOS/ARM64, and this function does not say so by
/// returning `false` there.** It is not `cfg`-gated: on Windows and on
/// macOS/ARM64 it still answers `true` for `CRATONVM_JIT_POISON_FREE=1`, but the
/// `platform_free` arms for those targets never consult it, so nothing is
/// poisoned and [`POISONED_JIT_BYTES`] stays zero. The operator-facing consumer
/// in `vm/src/jit/code_cache_lifecycle.rs` prints its poison line only for a
/// non-zero byte count, so on those hosts a run with the flag set is
/// indistinguishable from a run without it — the mode silently does nothing.
///
/// Stated here rather than only in `REVIEW-NOTE 4` four thousand lines below,
/// which is where the restriction was written down ("On Unix with
/// `CRATONVM_JIT_POISON_FREE=1`...") and is not where a caller of this function
/// reads. A Windows operator chasing a use-after-free with this flag is the
/// person who needs the sentence.
pub fn jit_poison_free_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_POISON_FREE")
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

/// Total bytes retired via the poison path.
pub static POISONED_JIT_BYTES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
fn platform_make_executable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    const PROT_READ: i32 = 1;
    const PROT_EXEC: i32 = 4;

    extern "C" {
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    }

    // Every architecture except x86/x86-64 has a split, non-coherent I-cache
    // and D-cache. After writing instructions via the data path we MUST flush
    // the range before allowing execution, otherwise the CPU may fetch stale
    // bytes (or even predecoded garbage) for every JIT-compiled method.
    //
    // We do this BEFORE flipping to PROT_EXEC: while the page is still RW
    // there is no chance of an instruction fetch racing the flush, and
    // `__clear_cache` does not require execute permission.
    //
    // x86-64/x86 has a coherent I-cache/D-cache (Intel SDM Vol.3 §11.6, AMD APM
    // Vol.2 §7.6.1) and only requires a serialising instruction on the
    // executing thread (any branch suffices). No explicit flush is needed
    // there, which is why `flush_icache_range_unix` compiles to nothing on it.
    //
    // GATING DISCIPLINE. This used to be an ALLOW-LIST of two operating
    // systems — `all(target_arch = "aarch64", any(target_os = "linux",
    // target_os = "freebsd"))` — and an allow-list leaves a new target
    // *silently uncovered*. What it missed:
    //
    //   * `aarch64-linux-android`, whose `target_os` is **"android"**, not
    //     "linux". On Android/ARM64 this function performed the `mprotect` and
    //     no cache maintenance whatsoever, so a freshly published method could
    //     be fetched from stale I-cache lines: a SIGILL, or a branch into the
    //     middle of whatever body previously occupied those bytes.
    //   * ARM64 NetBSD / OpenBSD / illumos.
    //   * every 32-bit `target_arch = "arm"` target.
    //   * RISC-V, where the required primitive is `fence.i` — which is exactly
    //     what `__clear_cache` expands to there.
    //
    // The Windows half of this same operation already got the discipline right
    // by EXCLUDING x86 rather than listing what needs the flush
    // (`platform_make_executable` for `target_os = "windows"`), so a new
    // ARM-ish target is covered by default. Both halves now use that one
    // discipline: the call site is unconditional and the *helper* is what is
    // architecture-gated, down to an empty body on x86.
    //
    // SAFETY: `ptr..ptr+size` is the mapping `platform_alloc` returned, so the
    // end pointer `flush_icache_range_unix` computes stays one-past-the-end of
    // that allocation.
    unsafe {
        flush_icache_range_unix(ptr, size);
    }

    // SAFETY: a mapping this module owns; `mprotect` changes only its
    // protection and reports failure through its return value.
    let ret = unsafe { mprotect(ptr, size, PROT_READ | PROT_EXEC) };
    if ret != 0 {
        Err(JitError::ProtectFailed(last_os_error_code()))
    } else {
        Ok(())
    }
}

/// Flush a range of CPU instruction cache lines covering `[ptr, ptr+size)`,
/// on every Unix target whose caches are not coherent.
///
/// Required on *every* architecture except x86/x86-64, where the data cache
/// (used by the JIT writer) and the instruction cache (used by the fetch unit)
/// are separate and not kept in sync by hardware. macOS-arm64 does not come
/// through here — it has its own `platform_make_executable` built on
/// `sys_icache_invalidate`, which is the only spelling Apple's libc exposes.
///
/// Implementation: we link the compiler builtin `__clear_cache`, which is the
/// portable spelling of "I just wrote instructions through the data path".
/// The compiler expands it per target:
///
/// * aarch64 (GCC and clang): `DC CVAU` + `DSB ISH` + `IC IVAU` (per line,
///   using `CTR_EL0` for the line size) + `DSB ISH` + `ISB` — exactly the
///   sequence ARM ARM B2.4.4 requires of the writer.
/// * 32-bit arm: the same shape, or the `cacheflush` syscall on Linux.
/// * riscv: `fence.i`, or Linux's `__riscv_flush_icache`.
///
/// The symbol is supplied by libgcc on GNU/Android targets and by compiler-rt
/// on the BSDs and on musl; both are linked by default for a Rust binary that
/// uses the standard library.
///
/// Signature follows the GCC builtin: `void __clear_cache(char *begin,
/// char *end)`. End is *exclusive*.
///
/// # Safety
///
/// `ptr..ptr+size` must lie within one allocation, so that `ptr.add(size)` is
/// in bounds or one past the end.
#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64")),
    not(any(target_arch = "x86", target_arch = "x86_64"))
))]
unsafe fn flush_icache_range_unix(ptr: *mut u8, size: usize) {
    extern "C" {
        fn __clear_cache(begin: *mut core::ffi::c_char, end: *mut core::ffi::c_char);
    }
    let begin = ptr as *mut core::ffi::c_char;
    // SAFETY: the caller guarantees the range is one allocation, so the end
    // pointer is at most one past its end.
    let end = unsafe { ptr.add(size) } as *mut core::ffi::c_char;
    // SAFETY: `__clear_cache` only issues cache-maintenance instructions over
    // the address range; it neither reads nor writes the bytes.
    unsafe { __clear_cache(begin, end) };
}

/// [`flush_icache_range_unix`] on x86/x86-64, where it is a no-op.
///
/// Intel SDM Vol.3 §11.6 and AMD APM Vol.2 §7.6.1 both guarantee a coherent
/// instruction cache: a store to a location that is subsequently fetched as an
/// instruction is observed by the fetch unit of the storing processor with no
/// explicit cache maintenance, and the only requirement is a serialising
/// operation on the executing thread — which the branch into the new code, and
/// the `mprotect` syscall ahead of it, both are.
///
/// This exists as a separate body rather than as a `cfg` on the call site so
/// the ONE place that decides "does this target need a flush" is here. See the
/// gating-discipline note in `platform_make_executable`: the previous shape
/// gated the call and silently omitted whole targets.
///
/// # Safety
///
/// Trivially safe; the signature matches the real implementation so the call
/// site does not need a `cfg`.
#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64")),
    any(target_arch = "x86", target_arch = "x86_64")
))]
unsafe fn flush_icache_range_unix(ptr: *mut u8, size: usize) {
    let _ = (ptr, size);
}

#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
fn platform_make_writable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;

    extern "C" {
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    }

    // SAFETY: a mapping this module owns; `mprotect` changes only its
    // protection and reports failure through its return value.
    let ret = unsafe { mprotect(ptr, size, PROT_READ | PROT_WRITE) };
    if ret != 0 {
        Err(JitError::ProtectFailed(last_os_error_code()))
    } else {
        Ok(())
    }
}

/// Data cells need no special mapping off macOS/ARM64: see the macOS arm's
/// `platform_alloc_data` for why that one platform must not share the code
/// allocator.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn platform_alloc_data(size: usize) -> Option<*mut u8> {
    platform_alloc(size)
}

// ---------------------------------------------------------------------------
// Code-adjacent data cells
// ---------------------------------------------------------------------------

/// Whether `near_globals` owns code/data placement in this process.
///
/// Always `false` where that module is not built: Windows lands in reach
/// without help and macOS/ARM64 allocates `MAP_JIT` pages on the kernel's
/// terms, so neither has the strategy and neither has the conflict.
#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
fn near_globals_engaged() -> bool {
    near_globals::engaged()
}

#[cfg(any(
    target_os = "windows",
    all(target_os = "macos", target_arch = "aarch64")
))]
fn near_globals_engaged() -> bool {
    false
}

/// One cell: a full cache line, so a cell can never share a line with an
/// unrelated one and a writer of one cannot invalidate another's.
///
/// x86-64's destructive-interference size, and the same 64 the two in-tree
/// `#[repr(align(64))]` precedents (`gc/src/zgc/census.rs`,
/// `gc/src/collector.rs`) already use.
pub const CODE_ADJACENT_CELL_SIZE: usize = 64;

/// How much address space one arena chunk covers.
///
/// 64 KiB rather than a single 4 KiB page because that is Windows' allocation
/// GRANULARITY: `VirtualAlloc(NULL, 4096, ...)` reserves 64 KiB regardless and
/// wastes the other 60 KiB of address space, so asking for less buys nothing.
/// On Unix the extra pages are demand-faulted and cost one page-table entry
/// each until touched. At [`CODE_ADJACENT_CELL_SIZE`] per cell that is 1024
/// cells per chunk, so in practice a process allocates exactly one chunk.
const ARENA_CHUNK: usize = 64 * 1024;

/// `(bump cursor, chunk end)`. `(0, 0)` means "no chunk yet".
static CELL_ARENA: std::sync::Mutex<(usize, usize)> = std::sync::Mutex::new((0, 0));

/// Hand out one zero-filled, cache-line-isolated, never-unmapped
/// [`CODE_ADJACENT_CELL_SIZE`]-byte cell for a word that JIT-compiled code
/// READS DIRECTLY, from the same OS primitive [`alloc_executable`] uses.
///
/// # Why the allocator matters
///
/// A word compiled code polls wants to be within ±2 GB of the code that polls
/// it, because that is the reach of x86-64's `disp32` and therefore the
/// difference between the one-instruction, no-register RIP-relative form and a
/// materialize-the-address fallback (`MOV r64, imm64` + the access: 15 bytes
/// and a clobbered register instead of 7 and none). Nothing FAILS when the
/// word is out of reach — every emitter that wants the short form checks the
/// delta and falls back — so the cost is silent, and no test notices.
///
/// The Rust heap cannot supply that adjacency. The default allocator here is
/// mimalloc (`vm-cli/src/main.rs`), which reserves its arenas from an address
/// range unrelated to the one an anonymous `mmap` / `VirtualAlloc(NULL, ...)`
/// hands out; measured on Linux, a `GcBarrier` field and a code buffer sat
/// ~124 TB apart. Two allocations from THE SAME primitive land in the same
/// region of the address space, which is what puts them in `disp32` range.
///
/// This is a placement HINT, not a guarantee: the OS chooses, and a caller
/// must stay correct when it chooses badly. Every caller therefore keeps its
/// out-of-reach fallback, and none may assume the cell is close to anything.
///
/// # Lifetime
///
/// Chunks are never unmapped and cells are never recycled, so an address
/// handed out here is valid for the rest of the process and can be baked into
/// generated code as an immediate. The cost is
/// [`CODE_ADJACENT_CELL_SIZE`] bytes per cell ever requested, permanently —
/// affordable only because callers are per-VM-ish singletons, not per-object.
/// Do not call this from anything that runs more than a bounded number of
/// times.
///
/// # It defers to `near_globals`, and must
///
/// There are two strategies for getting a polled word and the code that polls
/// it into one `disp32` window, and they point in opposite directions. This one
/// moves the WORD to the code's allocator. [`near_globals`] moves the CODE to
/// the allocator the VM's globals already live in, by hinting `mmap` — and it
/// anchors on `layout_replace_epoch_guard()`, relying on every other polled
/// cell being a few hundred megabytes away in that same mimalloc band.
///
/// Handing out a cell while that strategy is engaged would take the safepoint
/// flag OUT of the band the anchor is pulling the code towards, and leave the
/// poll ~130 TB behind — the precise failure `82bf52efd` describes for the
/// epoch counter, in the other cell. So this returns `None` whenever
/// `near_globals` owns placement, and the caller's ordinary-allocation fallback
/// is then the CORRECT answer rather than a degraded one.
///
/// Whoever owns placement must own it for every cell at once. Today that is
/// `near_globals` when `CRATONVM_JIT_CODE_NEAR_GLOBALS` is set and this
/// allocator otherwise.
///
/// Returns `None` if the OS refuses the mapping, or if `near_globals` is
/// engaged; a caller must then fall back to ordinary allocation. In the first
/// case it accepts the long encoding, in the second it is how it gets the short
/// one.
pub fn alloc_code_adjacent_cell() -> Option<*mut u8> {
    if near_globals_engaged() {
        return None;
    }
    // A poisoned arena mutex would mean a panic while holding it, and the only
    // thing done under it is integer arithmetic plus `platform_alloc` — take
    // the lock back rather than propagating a `None` that would silently
    // downgrade every later caller's encoding.
    let mut arena = CELL_ARENA.lock().unwrap_or_else(|e| e.into_inner());
    let (cursor, end) = *arena;
    if cursor == 0 || cursor + CODE_ADJACENT_CELL_SIZE > end {
        // `platform_alloc` returns page-aligned (Unix) or 64 KiB-granule
        // aligned (Windows) memory, both a multiple of the cell size, so the
        // bump cursor stays cache-line aligned without any rounding here.
        let base = platform_alloc_data(ARENA_CHUNK)? as usize;
        *arena = (base, base + ARENA_CHUNK);
    }
    let cell = arena.0;
    arena.0 += CODE_ADJACENT_CELL_SIZE;
    // Fresh anonymous pages are zero-filled by both `mmap(MAP_ANONYMOUS)` and
    // `VirtualAlloc(MEM_COMMIT)`, so this is redundant on the first cell of a
    // chunk. Written anyway: the guarantee a caller needs is "this cell reads
    // as zero", and making it depend on which cell of which chunk it happened
    // to get is the kind of thing that holds until someone adds recycling.
    // SAFETY: `cell` is the cell just carved out of a mapping this arena owns
    // and never releases; the bump cursor guarantees it is in bounds and that
    // no other reference to it exists.
    unsafe { std::ptr::write_bytes(cell as *mut u8, 0, CODE_ADJACENT_CELL_SIZE) };
    Some(cell as *mut u8)
}

// ---------------------------------------------------------------------------
// The JIT code arena
// ---------------------------------------------------------------------------
//
// What was wrong
// --------------
//
// Every compiled method allocated its own mapping. `jit/src/lib.rs` states the
// consequence in the round-8 TODO on `JitCache`: on Windows,
// `VirtualAlloc(NULL, ...)` rounds a reservation up to the ALLOCATION
// GRANULARITY, which is 64 KiB, while a typical method body is around 5 KiB. A
// JDK workload with ~3000 hot methods therefore reserves ~192 MiB of address
// space to hold ~15 MiB of code. That figure is the TODO's; nothing in this
// change measured it, and nothing in this change can — see "What is NOT
// claimed" below.
//
// Unix is not immune but is much less exposed: `mmap` rounds to the page (4 KiB
// on x86-64, 16 KiB on Apple Silicon), and the untouched tail of a mapping is
// demand-faulted, so the waste there is page-table entries and VMA count rather
// than committed memory. The Windows tax is the one worth closing.
//
// The shape of the fix
// --------------------
//
// One or a few large regions, each a single `platform_alloc`, carved into
// page-sized blocks by a bump cursor, with freed blocks recycled through a
// size-bucketed free list. The 64 KiB granularity tax is then paid once per
// REGION instead of once per method: at the default region size below, 16 MiB
// of code costs one 64 KiB rounding instead of 3000 of them.
//
// W^X, which is the whole difficulty
// ----------------------------------
//
// `ExecutableBuffer::finalize` calls `make_executable(self.ptr,
// self.capacity)`, and `make_writable` is the mirror image. Both are
// `VirtualProtect` / `mprotect`, and BOTH OPERATE ON WHOLE PAGES: the kernel
// rounds the start down and the end up. If two compiled methods shared a page,
// flipping one to RX would flip the other to RX at the same instant — including
// a method that is mid-emit — and flipping one back to RW for patching would
// make an executing method's page writable. Neither is a theoretical worry:
// relocation patching (`ir_lower`'s `patch_or_bail` family) and inline-cache
// updates call `make_writable` on published bodies while other threads run.
//
// This arena closes that by ROUNDING EVERY BLOCK UP TO A WHOLE PAGE — option
// (4) in the round-8 plan, in its first form. The argument, in full:
//
//   1. A region's `base` is `platform_alloc`'s return rounded UP to a page
//      boundary, and its `end` is the mapping's end rounded DOWN to one. So
//      `base` is page-aligned and `end - base` is a multiple of the page size.
//   2. The bump cursor starts at `base` and only ever advances by page
//      multiples (`alloc` rounds every request up before it touches the
//      cursor). By induction every address the cursor hands out is page-aligned
//      and every block's length is a whole number of pages.
//   3. The free list holds only ranges the cursor previously handed out, or
//      SPLIT PIECES of them. A split takes an entry of `m` pages at a
//      page-aligned base and cuts it at `n` pages: the front is `[addr,
//      addr + n*page)` and the back is `[addr + n*page, addr + m*page)`, both
//      page-aligned and both a whole number of pages. So recycled and split
//      blocks alike inherit (2), and the induction is closed under splitting.
//   4. Blocks from the bump cursor are disjoint by construction (the cursor is
//      monotone); a block on the free list is not live, and `release` removes
//      it from `live` before it can be handed out again, so no address is live
//      twice. A split removes its whole source entry from the free list before
//      either piece exists and then adds back exactly the tail, so the two
//      pieces of a split are disjoint and neither overlaps anything else.
//   5. A region is unmapped only when it holds no live block and every byte it
//      ever handed out is back on a free list, and every free entry naming it
//      is deleted before the `platform_free` — so reclamation cannot unmap a
//      page any live block is on, and cannot leave an address on a free list
//      that points at unmapped memory.
//
//   Therefore `make_executable(block.ptr, block.size)` touches exactly the
//   pages `[block.ptr, block.ptr + block.size)`, that range is page-aligned at
//   both ends, and by (4) no other LIVE block has any byte in it. There is no
//   page shared between two live blocks, so no protection flip can reach a
//   block other than its own.
//
//   None of this is left as prose. `every_arena_block_is_page_sized_and_page_
//   aligned` asserts (2) for every block a region hands out;
//   `make_executable_on_one_arena_block_cannot_reach_its_neighbour` asserts the
//   disjointness the conclusion rests on, and then performs a real protection
//   flip against a real neighbour; `a_split_block_serves_a_smaller_request_and_
//   its_remainder_is_reusable` asserts (3) and (4) for both pieces of a split;
//   and `a_reclaimed_regions_blocks_are_gone_from_the_free_list` asserts (5).
//
// The cost is the padding: a 5 KiB method occupies 8 KiB. That padding is NOT
// thrown away — `ExecutableBuffer::new_in` sets the buffer's capacity to the
// rounded size, so codegen may use it, and a method that would have overflowed
// a tight estimate by a few hundred bytes now fits. It is still counted as
// padding in the census, because from the arena's point of view it is space a
// caller did not ask for.
//
// Why NOT the batched-protect alternative
// ---------------------------------------
//
// The round-8 plan's preferred option was to batch RX transitions until a
// JIT-quiesce point so one `VirtualProtect` could cover many buffers. That
// changes WHEN code becomes executable relative to when it is published, which
// is an ordering the rest of the JIT depends on (`finalize` is called before
// the artifact reaches any dispatch surface, and a body that is published but
// not yet RX is a fault on the next call into it). It is a correctness change
// dressed as an optimisation, and it is deliberately not built here. Page
// rounding recovers the 64 KiB -> 4 KiB step, which is 15/16ths of the address
// space the TODO's arithmetic is about.
//
// macOS on Apple Silicon: INERT, deliberately
// -------------------------------------------
//
// That platform maps code `RW|EXEC` with `MAP_JIT` once and uses
// `pthread_jit_write_protect_np` as a THREAD-WIDE toggle; `platform_make_
// executable` there changes no page permission at all. Page sharing is
// therefore harmless on macOS — the arena would be sound — but the thing the
// arena exists to recover, Windows' 64 KiB allocation granularity, does not
// exist there either, and the risks that would be new are unmeasured: a single
// 16 MiB `MAP_JIT` region under the hardened runtime, and 16 KiB pages that
// make the rounding padding four times what it is elsewhere. Nothing in this
// session can measure any of that, so [`JIT_CODE_ARENA_IS_INERT`] is `true`
// there and every `alloc` answers with a standalone mapping — byte-for-byte
// what `ExecutableBuffer::new` does today. macOS behaviour is unchanged, and
// that is a statement about code, not an intention: `JitCodeArena::alloc`'s
// first branch is `self.inert`.
//
// What is NOT claimed
// -------------------
//
// No saving here has been measured. The 177-192 MiB figure is the round-8
// TODO's arithmetic, carried forward unverified; this change did not run a
// JDK workload, or any workload. That is why the arena is default-OFF behind
// `CRATONVM_JIT_CODE_ARENA` and why nothing in `jit/src/lib.rs` calls
// `new_in` yet. The census below exists so that the first person who does turn
// it on gets numbers instead of another estimate.

/// Default bytes per code region — 16 MiB, the size the round-8 plan proposed.
///
/// Three things pin it. (a) It must be much larger than the 64 KiB Windows
/// allocation granularity for the arena to be worth having at all; 16 MiB pays
/// that tax once per 256 methods-worth of code. (b) It must be small enough
/// that a process which compiles a handful of methods does not reserve
/// absurdly more than it uses — 16 MiB is 6.25% of the default code-cache cap
/// (`DEFAULT_JIT_CODE_CACHE_CAP_BYTES`, 256 MiB), so the first region is a
/// rounding error against a budget the VM already accepts. (c) It keeps the
/// linear scans in [`JitCodeArena::bump`] and [`free_if_arena`] cheap and means
/// the span table never needs an index — 256 MiB of code at 16 MiB a region is
/// 16 regions, so a scan over live regions is over a handful of slots.
///
/// **Reason (c) is an expectation, not an enforced bound, and it used to be
/// written as one** ("the cap divided by the region size bounds the region count
/// at 16"). Nothing in this module consults the code-cache cap — see
/// [`JitCodeArena::bump`]'s doc for the full account and for why the indirect
/// route through `COMMITTED_JIT_CODE_BYTES` does not close the gap either. The
/// arithmetic above is the shape a well-behaved workload has, and the scans are
/// cheap because of it; it is not a guarantee the code makes, and a workload
/// that fragments badly enough to hold many more live regions would make those
/// scans linear in something this file does not limit.
///
/// This is also the granularity at which address space is committed to code
/// and, since a region is the unit [`JitCodeArena::reclaim_region`] gives back,
/// the granularity at which it is returned. The hysteresis rule keeps one empty
/// region, so 16 MiB is the floor a quiesced arena settles at rather than zero;
/// see [`JIT_CODE_ARENA_EMPTY_REGION_SPARES`].
pub const JIT_CODE_DEFAULT_REGION_BYTES: usize = 16 * 1024 * 1024;

/// `true` on macOS/ARM64, where the arena declines to carve and every
/// allocation is a standalone mapping. See the module section above.
pub const JIT_CODE_ARENA_IS_INERT: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));

/// The page size the arena rounds blocks to, if the OS reports a plausible one.
///
/// Queried rather than hard-coded, because the W^X argument above is only valid
/// when this is at least the real MMU page size: Apple Silicon and some ppc64
/// and aarch64 Linux kernels use 16 KiB or 64 KiB pages, and a 4 KiB constant
/// there would let two arena blocks share a real page while every assertion in
/// this file still passed.
///
/// Over-estimating is safe (more padding, same soundness); under-estimating is
/// not, which is why [`probe_code_page_size`]'s fallback is 64 KiB and not
/// 4 KiB.
pub fn code_page_size() -> usize {
    static PAGE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *PAGE.get_or_init(probe_code_page_size)
}

/// The page size to assume when the OS reports something this module does not
/// believe. 64 KiB is the largest page size in use on any target this crate
/// builds for, and is also Windows' allocation granularity, so assuming it can
/// never be an under-estimate on a host we run on.
const CONSERVATIVE_PAGE_FALLBACK: usize = 64 * 1024;

fn probe_code_page_size() -> usize {
    let raw = raw_page_size();
    if (4096..=1024 * 1024).contains(&raw) && raw.is_power_of_two() {
        return raw;
    }
    tracing::warn!(
        reported = raw,
        fallback = CONSERVATIVE_PAGE_FALLBACK,
        "jit code arena: the OS reported an implausible page size; assuming the \
         conservative fallback instead. Over-estimating costs padding; \
         under-estimating would let two arena blocks share a page, which is the \
         one thing the arena's W^X argument forbids."
    );
    CONSERVATIVE_PAGE_FALLBACK
}

#[cfg(target_os = "windows")]
fn raw_page_size() -> usize {
    /// `SYSTEM_INFO` from `<sysinfoapi.h>`, with the leading union spelled as
    /// its `wProcessorArchitecture` / `wReserved` arm (the `dwOemId` arm is the
    /// same four bytes). Layout on x86-64: two `WORD`s, a `DWORD`, two
    /// pointers, a `DWORD_PTR`, three `DWORD`s, two `WORD`s — 48 bytes,
    /// 8-byte aligned, which is what `#[repr(C)]` reproduces.
    #[repr(C)]
    struct SystemInfo {
        w_processor_architecture: u16,
        w_reserved: u16,
        dw_page_size: u32,
        lp_minimum_application_address: *mut core::ffi::c_void,
        lp_maximum_application_address: *mut core::ffi::c_void,
        dw_active_processor_mask: usize,
        dw_number_of_processors: u32,
        dw_processor_type: u32,
        dw_allocation_granularity: u32,
        w_processor_level: u16,
        w_processor_revision: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemInfo(lpSystemInfo: *mut SystemInfo);
    }

    // SAFETY: `GetSystemInfo` writes exactly one `SYSTEM_INFO` through the
    // pointer and reads nothing, so a zeroed local of the matching `#[repr(C)]`
    // layout is a valid destination. Every field is a plain integer or an
    // address the caller never dereferences, so the all-zero pattern is a valid
    // (if meaningless) value of the type before the call fills it in.
    let info: SystemInfo = unsafe {
        let mut info: SystemInfo = std::mem::zeroed();
        GetSystemInfo(&mut info);
        info
    };
    // Touch the fields the arena does not use, so a layout mistake that
    // silently shifts `dw_page_size` is at least visible to a reader here
    // rather than only as a wrong page size. `dwAllocationGranularity` is the
    // 64 KiB number this whole module exists because of.
    debug_assert!(
        info.dw_allocation_granularity >= info.dw_page_size,
        "Windows allocation granularity ({}) below page size ({}): the \
         SYSTEM_INFO layout in this function does not match the OS",
        info.dw_allocation_granularity,
        info.dw_page_size
    );
    let _ = (
        info.w_processor_architecture,
        info.w_reserved,
        info.lp_minimum_application_address,
        info.lp_maximum_application_address,
        info.dw_active_processor_mask,
        info.dw_number_of_processors,
        info.dw_processor_type,
        // Also read here and not only in the `debug_assert` above, which is
        // compiled out of a release build and would leave this field unread.
        info.dw_allocation_granularity,
        info.w_processor_level,
        info.w_processor_revision,
    );
    info.dw_page_size as usize
}

/// `getpagesize()` rather than `sysconf(_SC_PAGESIZE)`: the latter's argument
/// constant is not portable (30 on Linux, 29 on Darwin and the BSDs), and
/// getting it wrong would return an unrelated sysconf value that
/// [`probe_code_page_size`] would then have to reject. `getpagesize` takes no
/// argument and so cannot be mis-called; it is present in glibc, musl, Bionic,
/// Darwin's libSystem and every BSD libc.
#[cfg(not(target_os = "windows"))]
fn raw_page_size() -> usize {
    extern "C" {
        fn getpagesize() -> i32;
    }
    // SAFETY: takes no arguments, touches no memory, and returns a plain int.
    let raw = unsafe { getpagesize() };
    if raw <= 0 {
        // Signalled as implausible; `probe_code_page_size` takes the fallback.
        return 0;
    }
    raw as usize
}

/// Where a block came from, which is what decides how it is returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockOrigin {
    /// Carved out of `regions[i]`. Returned to the free list, never to the OS.
    Region(usize),
    /// Its own `platform_alloc` mapping, because the arena is inert on this
    /// platform, the request was larger than a region, or the OS refused a new
    /// region. Returned to the OS, exactly as a non-arena buffer is.
    Standalone,
}

/// One block of executable memory handed out by a [`JitCodeArena`].
///
/// `size()` is the PAGE-ROUNDED length, which is both what the owner may write
/// and what `make_executable` will be called with. `requested()` is what the
/// caller asked for; the difference is the padding the W^X argument buys.
#[derive(Debug)]
pub struct ArenaBlock {
    ptr: *mut u8,
    size: usize,
    requested: usize,
    origin: BlockOrigin,
}

// SAFETY: an `ArenaBlock` is a plain (address, length) pair describing memory
// that nothing else holds a reference to — the arena removed it from its bump
// cursor or free list before constructing this, and will not hand the same
// address out again until the block is returned. Moving that description to
// another thread moves the exclusive right to write those bytes, which is the
// same argument `ExecutableBuffer`'s own `unsafe impl Send` makes.
unsafe impl Send for ArenaBlock {}

impl ArenaBlock {
    /// Base address. Page-aligned for every origin.
    #[inline]
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Page-rounded length — the range `make_executable` may be called with.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// `true` only for a block a future caller managed to ask zero bytes for,
    /// which [`JitCodeArena::alloc`] refuses; present because `size()` reads as
    /// a length and clippy asks for the pair.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// What the caller asked for, before page rounding.
    #[inline]
    pub fn requested(&self) -> usize {
        self.requested
    }

    /// Whether this came out of a region or is its own mapping.
    #[inline]
    pub fn origin(&self) -> BlockOrigin {
        self.origin
    }
}

/// One `platform_alloc` mapping, carved by a bump cursor.
///
/// `base`/`end` are the mapping's page-aligned interior; `mapped_base`/
/// `mapped_len` are the mapping itself, kept so the census can report the head
/// and tail bytes alignment cost (zero on every platform measured, because both
/// allocators already return page-aligned memory, but the arena does not want
/// to depend on that).
///
/// The four counters are what makes [`JitCodeArena::reclaim_region`] possible
/// without a scan: a region may be unmapped only when nothing it ever handed
/// out is still out, and "nothing is still out" is `live_blocks == 0 &&
/// stranded_bytes == 0 && free_bytes == handed_out()`. Each is maintained at
/// exactly one place — `claim` and `release` for `live_blocks`,
/// [`JitCodeArena::push_free`] and [`JitCodeArena::pop_free`] for `free_blocks`
/// and `free_bytes`, `release`'s protection-restore failure arm for
/// `stranded_bytes` — so that a split, which pops one entry and pushes a
/// smaller one back, cannot make them drift.
struct Region {
    mapped_base: usize,
    mapped_len: usize,
    base: usize,
    cursor: usize,
    end: usize,
    /// Blocks carved out of this region and not yet returned.
    live_blocks: usize,
    /// Entries on some `free_by_pages[n]` that name this region.
    free_blocks: usize,
    /// Page-rounded bytes of this region currently on a free list.
    free_bytes: usize,
    /// Bytes handed out of this region that can never come back, because
    /// restoring them to writable failed. Non-zero pins the region forever;
    /// see [`JitCodeArena::release`].
    stranded_bytes: usize,
}

impl Region {
    /// Bytes the bump cursor has ever handed out of this region.
    #[inline]
    fn handed_out(&self) -> usize {
        self.cursor - self.base
    }

    /// Bytes still ahead of the bump cursor.
    #[inline]
    fn bump_room(&self) -> usize {
        self.end - self.cursor
    }

    /// Whether every byte this region ever handed out is back on a free list,
    /// so unmapping it cannot pull the ground out from under a live block.
    ///
    /// A region that was never bumped satisfies this too (`handed_out()` and
    /// `free_bytes` are both zero), which is deliberate: a region mapped for an
    /// allocation that then took a different path is exactly as reclaimable as
    /// one that emptied out.
    #[inline]
    fn is_reclaimable(&self) -> bool {
        self.live_blocks == 0 && self.stranded_bytes == 0 && self.free_bytes == self.handed_out()
    }
}

/// A block currently handed out of a region.
struct LiveBlock {
    /// Page-rounded length.
    size: usize,
    /// What the caller asked for.
    requested: usize,
    /// Index into `regions`, so a recycled block remembers where it came from.
    region: usize,
}

/// How many empty regions a [`JitCodeArena`] keeps mapped rather than
/// reclaiming.
///
/// One. The rule it parameterises — reclaim only when MORE than this many
/// regions are empty, then reclaim down to exactly this many — is argued in
/// [`JitCodeArena`]'s hysteresis section. The short version: unmapping the only
/// empty region and re-mapping it on the next compile costs strictly more than
/// keeping it, so the arena must acquire a second empty region before it will
/// give up a first.
///
/// A constant and not a flag, deliberately. A knob here would be a knob nobody
/// could set well: the number that matters is the peak-to-steady-state ratio of
/// a workload's code footprint, which this module cannot see and which no
/// measurement in this repository has produced. If a later run produces one,
/// the follow-up is named in the REVIEW-NOTE at the bottom of this file.
pub const JIT_CODE_ARENA_EMPTY_REGION_SPARES: usize = 1;

/// What a [`JitCodeArena`] can tell an eviction policy about itself.
///
/// Deliberately a snapshot of PRESSURE and not of layout: an eventual policy
/// wants to know how much address space is committed, how much of it is
/// actually in use, and whether the arena is about to want more. The full
/// partition is [`JitCodeArenaCensus`], which costs O(live blocks) and is meant
/// for a report point rather than for the allocation path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitCodeArenaPressure {
    /// Which arena, matching [`JitCodeArenaCensus::arena_id`].
    pub arena_id: u64,
    /// Address space taken from the OS for regions. The number a policy is
    /// trying to keep down.
    ///
    /// "Reserved" is the Unix reading. On Windows `platform_alloc` passes
    /// `MEM_COMMIT | MEM_RESERVE`, so a region is COMMITTED in full the moment
    /// it is mapped and these bytes are charged against the system commit limit
    /// whether or not any method has been written into them. That is a
    /// pre-existing property of `platform_alloc` and not something the arena
    /// introduced, but it is the reason a 16 MiB region is a larger promise on
    /// Windows than the word "reserved" suggests, and it is why the hysteresis
    /// spare is one region and not four.
    pub bytes_reserved: usize,
    /// Page-rounded bytes currently handed out of regions.
    pub bytes_live: usize,
    /// Page-rounded bytes sitting on the size-class free lists — reserved,
    /// reusable, and not in use. A policy that evicts while this is large is
    /// evicting for nothing.
    pub bytes_free_listed: usize,
    /// Regions currently mapped.
    pub regions: usize,
    /// Bytes per region, so a policy can tell what "one more region" costs.
    pub region_bytes: usize,
    /// Live region blocks, i.e. compiled bodies the arena is holding memory
    /// for.
    pub live_blocks: usize,
    /// Whether the last RESOLVED allocation had to map a new region.
    ///
    /// "Resolved" matters: when an eviction hook reads this, the allocation
    /// that triggered the hook has not finished, so these two describe the
    /// PREVIOUS one. A hook that sees `last_alloc_mapped_region` is looking at
    /// an arena that has now grown twice in a row.
    pub last_alloc_mapped_region: bool,
    /// Whether the last resolved allocation fell back to a standalone mapping
    /// — the arena declining to serve it, because the platform is inert, the
    /// request was larger than a region, or the OS refused a region.
    pub last_alloc_was_standalone: bool,
}

/// A hook the owner installs to be asked to evict something before the arena
/// reserves more address space.
///
/// Returns `true` if the owner believes it freed something. The arena then
/// re-tries its free lists and its bump cursors before mapping a region, and
/// counts the call in [`JitCodeArenaCensus::evictions_performed`].
///
/// # This is a seam, and the lock is why it is only a seam
///
/// **The hook is called with the arena's mutex HELD**, from inside
/// [`JitCodeArena::alloc`]. The obvious implementation of an eviction policy —
/// pick a cold `CompiledMethod`, drop it — therefore deadlocks: dropping a
/// `CompiledMethod` drops its `ExecutableBuffer`, which calls
/// `free_executable`, which calls `free_if_arena`, which locks this same arena.
/// `std::sync::Mutex` is not reentrant, so that is a hang and not a panic.
///
/// A hook may therefore only SELECT victims and hand them to a queue the owner
/// drains after the allocation has returned. Which means `true` from such a
/// hook is a promise about the future, the retry that follows it will miss, and
/// the region gets mapped anyway — this allocation is not the one eviction
/// helps. That is an honest description of what is built here and it is why
/// the doc on [`JitCodeArena`] calls this a seam rather than a feature.
///
/// The fix, for whoever installs the first hook: move the call out of the lock.
/// `ExecutableBuffer::new_in` already brackets the lock explicitly, so it can
/// do `alloc`-without-growing, release the lock, ask, then `alloc` for real.
/// That is not built here because there is no installer to validate it against
/// and because it changes `alloc`'s contract, which the tests in this file
/// pin. It is the first item of the eviction REVIEW-NOTE at the bottom of this
/// file.
///
/// `Send` because the arena lives in an `Arc<Mutex<_>>` shared across compile
/// threads; not `Sync`, because the mutex is what serialises it.
pub type JitCodeArenaEvictionHook = Box<dyn Fn(&JitCodeArenaPressure) -> bool + Send + 'static>;

/// A per-VM arena of executable code regions.
///
/// # Lifetime: a region is unmapped only when it is provably empty
///
/// There is deliberately no `Drop` that unmaps regions. A block handed out of a
/// region can outlive the arena handle — an `ExecutableBuffer` holds only a raw
/// pointer, and this module cannot give it a back-pointer (see
/// [`free_executable`]) — so unmapping on drop would turn "the arena went away"
/// into a use-after-free of executable memory in every thread still inside a
/// compiled body. Leaking the regions in that case is the safe half of that
/// trade, and costs nothing in the shape that actually ships:
/// [`shared_jit_code_arena`] is a process singleton that is never dropped.
///
/// What IS unmapped is an individual region that has gone empty, by
/// [`Self::reclaim_region`]. "Empty" is [`Region::is_reclaimable`]: no live
/// block, no stranded block, and every byte the bump cursor ever handed out
/// back on a free list. The first shape of this arena unmapped nothing at all,
/// so a run that compiled a burst and then quiesced held every region it had
/// ever needed until the process exited; at 16 MiB a region that is a real
/// address-space figure, and it is the figure the arena exists to reduce.
///
/// ## Hysteresis: one empty region is kept as a spare
///
/// Reclamation is NOT eager, and the reason is that the eager version is a
/// pessimisation rather than a bug. A compile that empties the last region
/// would unmap it; the next compile would immediately `platform_alloc` 16 MiB
/// again, at a cost (a `VirtualAlloc` reservation, or an `mmap` plus the page
/// faults that first-touch the new pages) that is strictly larger than the cost
/// of having kept it. A workload that alternates between one and zero live
/// methods would pay that on every method.
///
/// The rule, which is what [`Self::maybe_reclaim_regions`] implements:
///
/// > Reclaim only when MORE than [`JIT_CODE_ARENA_EMPTY_REGION_SPARES`]
/// > regions are empty, and then reclaim down to exactly that many.
///
/// With the constant at 1: two empty regions are needed before anything is
/// unmapped, and one is left behind. So the arena has to acquire a second empty
/// region before it will give up a first, which is the whole of the hysteresis —
/// an oscillation around a single empty region never unmaps anything, and a
/// genuine quiesce from N regions gives back N-1. The spare kept is the empty
/// region with the most contiguous bump room (ties to the lowest slot, which is
/// the one [`Self::bump`] reaches first), because bump room is the only thing
/// that can serve a request of *any* size; an empty region whose space is all
/// on exact-size free lists can only serve the classes it happens to hold.
///
/// The cost of the rule, stated rather than hidden: a VM that compiles a
/// handful of methods and then runs forever holds one region — 16 MiB of
/// reserved address space at the default [`JIT_CODE_DEFAULT_REGION_BYTES`] —
/// for the life of the process, even with nothing compiled at all. That is
/// deliberate and it is the price of not thrashing. A caller that would rather
/// have the address space back has no knob for it today; adding one is a
/// follow-up and is named in the REVIEW-NOTE at the bottom of this file.
///
/// ## Reclamation STRENGTHENS the ordering obligation below — it does not
/// ## weaken it
///
/// The obligation in the next section exists because the arena re-hands an
/// address. Unmapping is the other way to stop using an address, and it is the
/// safer one: after `platform_free` the range is not mapped, so a stale jump or
/// a stale recovery-PC match lands on unmapped memory and the process takes a
/// fault with an address that has no provenance — loud, immediate, and
/// attributable. Re-handing the same address to a different method is the
/// quiet failure: the fault is *handled*, by a recovery PC that belonged to a
/// body that no longer exists, and control resumes somewhere meaningless inside
/// a live method. So per byte, replacing "re-hand" with "unmap" trades silent
/// wrong control flow for a crash.
///
/// But that is only true when the registries have already been purged, and
/// reclamation cannot happen any earlier than re-handing can: a region is
/// reclaimed only from inside [`Self::release`], which is only reached from
/// `ExecutableBuffer::drop`, which is downstream of every purge (see below).
/// A block whose registries were NOT purged is a block that is still live from
/// the arena's point of view, and a region with a live block is not
/// reclaimable. The obligation is therefore unchanged in content and its
/// violations are now *more* detectable, not less — which is why this section
/// says "strengthens": the same bug that used to produce a silent resume now
/// has a chance of producing a segfault instead.
///
/// One caveat, because it is a real weakening at the margin and pretending
/// otherwise would be the kind of comment this file exists not to have: on Unix
/// with `CRATONVM_JIT_POISON_FREE=1`, `platform_free` does
/// `mprotect(PROT_NONE)` instead of `munmap`, so a reclaimed region stays
/// mapped-but-inaccessible and its address is never reused. That is strictly
/// better for diagnosis and strictly worse for the address-space figure, and it
/// is the documented intent of that flag rather than an accident.
///
/// # THE ORDERING OBLIGATION ON WHOEVER FREES
///
/// **A block must not be returned to this arena until every address-keyed
/// registry has been purged of its range.** The arena WILL hand the same
/// address out again — to the next allocation of the same page count, or, since
/// splitting, to the next SMALLER one, which takes the returned block's leading
/// pages and leaves the rest on a free list. Either way a stale registry entry
/// now names memory that belongs to a different method. The registries that key
/// on a code address are:
///
/// * the implicit-null recovery table (`jit/src/implicit_null.rs`), whose
///   module doc names this exact hazard — "a JIT code buffer is freed and its
///   address REUSED" — and whose consequence is not a crash but "silent,
///   arbitrary control flow": a fault in the NEW method matches the OLD
///   method's recovery PC and the signal handler resumes somewhere it has no
///   business resuming;
/// * the JIT method-name table (`unregister_jit_method_name`), which would
///   otherwise attribute the new body's frames to the retired method;
/// * `jit_code_regions` (`validate_code_ptr`), which must not be left claiming
///   a range that the arena has taken back;
/// * the debugger/perf symbol sink (`crate::code_events::retire`).
///
/// Today that obligation is DISCHARGED, and by inheritance rather than by
/// anything in this file: the only path into [`Self::release`] is
/// [`free_executable`], whose only caller is `ExecutableBuffer::drop`, which
/// deregisters `jit_code_regions` and calls `code_events::retire` BEFORE it
/// frees — and whose owning `CompiledMethod::drop` runs its own body, including
/// `implicit_null::unregister_range` and `unregister_jit_method_name`, before
/// any field (and so before the buffer) is dropped at all. `release` carries a
/// `debug_assert` that re-checks the `jit_code_regions` half of this, because
/// that is the half this module can actually see.
///
/// Inheritance is not a guarantee, so: any future caller that constructs an
/// [`ArenaBlock`] some other way, or that calls [`Self::free`] directly, MUST
/// perform those purges first. There is no second line of defence. This is
/// restated in the `// REVIEW-NOTE:` at the bottom of the file because it is
/// the single thing most likely to be got wrong when `jit/src/lib.rs` is wired
/// up.
///
/// # The free list splits, and does not coalesce
///
/// `free_by_pages[n]` holds blocks of exactly `n` pages. A request for `n`
/// pages takes one out of `free_by_pages[n]` if there is one; otherwise it takes
/// one from the SMALLEST non-empty class above `n`, keeps the first `n` pages
/// and pushes the remaining `m - n` back into `free_by_pages[m - n]`. That is
/// [`Self::pop_free`], and it is what the
/// exact-fit-only first shape of this arena could not do: there, a 2-page
/// request could not be served by a 5-page free block, so a workload whose
/// method sizes drifted upward shattered the arena into classes nothing asked
/// for.
///
/// Splitting cannot weaken the W^X argument, and the reason is arithmetic
/// rather than care. The entry popped is page-aligned and a whole number of
/// pages (every entry is, by induction: the bump cursor only ever advances by
/// page-rounded amounts from a page-aligned base, and a split of a page-aligned
/// page-multiple at a page-multiple offset yields two of the same). The front
/// piece is `[addr, addr + n*page)` and the remainder is
/// `[addr + n*page, addr + m*page)`; both are page-aligned and page-sized, so
/// `make_executable` on either still touches that piece's pages and no other's.
/// And the two pieces cannot both be outstanding: the popped entry is removed
/// from the free list in full BEFORE either piece exists, the front becomes a
/// live block and the remainder becomes exactly one new free entry, so every
/// byte of the original is in exactly one of `live` and `free_by_pages`
/// afterwards, as it was before.
///
/// **Coalescing is still not implemented**, and that is the fragmentation that
/// remains. Blocks only ever get smaller: nothing merges `[a, a+page)` and
/// `[a+page, a+2*page)` back into a 2-page block, so a region that has been
/// churned for long enough approaches a free list of single pages that can
/// serve no multi-page request even though the region is mostly free. Two
/// things bound the damage, and neither is a fix:
///
/// * splitting only ever creates ONE new entry per allocation, and only when
///   the exact class missed, so the shattering rate is bounded by the
///   exact-class miss rate rather than by the allocation rate;
/// * region reclamation is a coarse coalescer. When a region goes entirely
///   free its whole free list is discarded with it (see
///   [`Self::reclaim_region`]) and the address space comes back unshattered,
///   so the shattering a region can accumulate is bounded by that region's
///   lifetime rather than by the process's.
///
/// The measurement that would say whether real coalescing is needed is
/// `bytes_free_listed` against `free_blocks` in [`JitCodeArenaCensus`] over a
/// long JDK-shaped run: a mean free-block size that decays towards one page
/// while `bytes_free_listed` stays large is fragmentation, and a mean that
/// holds is not. Nothing here has run that.
///
/// # Eviction: a seam, not a policy
///
/// [`Self::set_eviction_hook`] installs a callback the arena invokes when it is
/// about to reserve more address space. **It is unset by default and nothing in
/// this crate installs one**, so with no installer this is a seam and not a
/// feature: the behaviour of an arena with no hook is byte-for-byte what it was
/// before the hook existed. It is here because the alternative — inventing an
/// eviction policy in a module that cannot see an invocation count — is how you
/// get a policy nobody can justify. See [`JitCodeArenaEvictionHook`] for what
/// a hook may and may not do (it is called with the arena lock HELD), and the
/// REVIEW-NOTE at the bottom of this file for the VM-side data an eventual
/// policy would key on and where it lives.
pub struct JitCodeArena {
    /// Identity in the process-wide span table, from [`NEXT_ARENA_ID`].
    ///
    /// NOT `Arc::as_ptr` of the handle, which was the first shape and is
    /// unsound: a dead arena's spans are never removed from the table (a block
    /// can still come back after its arena is gone, and that free must be
    /// swallowed rather than sent to `platform_free` with an interior address),
    /// so an address-derived id could be RECYCLED by the allocator onto a new
    /// arena, which would then inherit a dead arena's spans and, if the region
    /// counts happened to match, never publish its own. A monotonic counter
    /// cannot be reused.
    id: u64,
    region_bytes: usize,
    /// Region slots. `None` is a slot whose region was reclaimed.
    ///
    /// # Why a tombstone and not a `remove`
    ///
    /// A region is named by its INDEX in three places — `LiveBlock::region`,
    /// the second half of every `free_by_pages` entry, and
    /// `BlockOrigin::Region(_)` — so compacting the vector on reclamation would
    /// silently renumber every one of them. Tombstoning keeps every surviving
    /// index correct for nothing, which is the only acceptable cost here.
    ///
    /// The slot is REUSED by the next [`Self::map_region`], which is sound
    /// exactly because reclamation requires the slot to be referenced by
    /// nothing: no live block (the region was empty), no free entry
    /// ([`Self::reclaim_region`] removes them first), and no outstanding
    /// `ArenaBlock`, since an `ArenaBlock` for a region block exists only while
    /// that block is live. So `regions.len()` is bounded by the high-water mark
    /// of simultaneously-mapped regions rather than by the number ever mapped.
    regions: Vec<Option<Region>>,
    /// `free_by_pages[n]` = `(base address, owning region index)` for free
    /// blocks of exactly `n` pages. Index 0 is always empty (a zero-byte
    /// allocation is refused, and a split that would leave a zero-page
    /// remainder is the exact-fit case and leaves none).
    free_by_pages: Vec<Vec<(usize, usize)>>,
    live: std::collections::HashMap<usize, LiveBlock>,
    served_from_free_list: u64,
    served_from_bump: u64,
    served_standalone: u64,
    region_map_failures: u64,
    unknown_frees: u64,
    /// Allocations served by splitting a larger free block. A subset of
    /// `served_from_free_list`, not a separate origin.
    blocks_split: u64,
    /// Regions unmapped by [`Self::reclaim_region`], and their mapped bytes.
    regions_reclaimed: u64,
    bytes_reclaimed: u64,
    /// Page-rounded bytes ever handed out, and the bytes callers asked for.
    /// Their difference is the cumulative rounding tax; see
    /// [`JitCodeArenaCensus::bytes_fragmentation_total`]. Both origins are
    /// counted, because a standalone mapping is page-rounded too.
    bytes_served_total: u64,
    bytes_requested_total: u64,
    /// Blocks and bytes that came back but could not be made writable again,
    /// so they are neither live nor free. See [`Self::release`].
    stranded_blocks: usize,
    bytes_stranded: usize,
    /// The eviction seam. `None` by default and nothing in this crate sets it;
    /// see [`JitCodeArenaEvictionHook`].
    evict: Option<JitCodeArenaEvictionHook>,
    evictions_requested: u64,
    evictions_performed: u64,
    /// How the most recently RESOLVED allocation ended. Read by
    /// [`Self::pressure`], and deliberately not cleared at the top of
    /// [`Self::alloc`]: when the eviction hook reads them mid-`alloc`, the
    /// current allocation has not resolved yet, so the honest answer is the
    /// previous one's.
    last_alloc_mapped_region: bool,
    last_alloc_was_standalone: bool,
    inert: bool,
}

impl Default for JitCodeArena {
    fn default() -> Self {
        Self::new()
    }
}

impl JitCodeArena {
    /// An arena with [`JIT_CODE_DEFAULT_REGION_BYTES`] regions.
    pub fn new() -> Self {
        Self::with_region_bytes(JIT_CODE_DEFAULT_REGION_BYTES)
    }

    /// An arena with a chosen region size, rounded down to a page and never
    /// below one page.
    ///
    /// Exists for two callers. The tests here use small regions so that
    /// "larger than a region" is a cheap request rather than a 16 MiB one. And
    /// the round-8 plan's item 5 wants a separate small-region arena for OSR
    /// trampolines (typically under 1 KiB), which would otherwise each burn a
    /// whole page out of the method arena — that arena is not created here, but
    /// the knob it needs is.
    pub fn with_region_bytes(region_bytes: usize) -> Self {
        let page = code_page_size();
        let region_bytes = region_bytes.max(page) & !(page - 1);
        Self {
            id: NEXT_ARENA_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            region_bytes,
            regions: Vec::new(),
            free_by_pages: Vec::new(),
            live: std::collections::HashMap::new(),
            served_from_free_list: 0,
            served_from_bump: 0,
            served_standalone: 0,
            region_map_failures: 0,
            unknown_frees: 0,
            blocks_split: 0,
            regions_reclaimed: 0,
            bytes_reclaimed: 0,
            bytes_served_total: 0,
            bytes_requested_total: 0,
            stranded_blocks: 0,
            bytes_stranded: 0,
            evict: None,
            evictions_requested: 0,
            evictions_performed: 0,
            last_alloc_mapped_region: false,
            last_alloc_was_standalone: false,
            inert: JIT_CODE_ARENA_IS_INERT,
        }
    }

    /// Bytes per region for this arena, after page rounding.
    #[inline]
    pub fn region_bytes(&self) -> usize {
        self.region_bytes
    }

    // `pub fn is_inert(&self) -> bool { self.inert }` was here and was deleted in
    // round 10 wave 8. It had no caller anywhere in the workspace: every reader
    // of "does this platform carve regions" reads the compile-time constant
    // `JIT_CODE_ARENA_IS_INERT` instead (`jit_code_arena_enabled`,
    // `JitCodeArena::census`, `log_jit_code_arena_census`), and the field the
    // accessor returned is only ever initialised *from* that constant, so the
    // two could not disagree. What the accessor did add was the impression that
    // inertness is per-arena state a caller might usefully interrogate, which it
    // is not.
    //
    // The `inert` FIELD stays: `JitCodeArena::alloc` reads it as the first term
    // of its "serve standalone" test, which is a real production read and the
    // reason the flag is materialised on the struct at all.
    //
    // See `docs/internal/fixed-bugs/r10-readers-platform-dead-public-surface-RESOLVED-20260922.md`
    // item 2.

    /// Hand out at least `size` bytes of writable, not-yet-executable memory.
    ///
    /// The returned block is page-aligned and page-sized, which is what makes
    /// `make_executable(block.as_ptr(), block.size())` touch that block's pages
    /// and no other live block's. `None` only when `size` is zero, when the
    /// rounding would overflow `usize`, or when the OS refuses even a
    /// standalone mapping — i.e. exactly the cases in which
    /// [`alloc_executable`] would also answer `None`.
    ///
    /// The order it tries, which is also the order the census's `served_*`
    /// counters partition allocations by: the free lists (exactly, then by
    /// splitting the smallest block that is large enough), then a bump cursor,
    /// then — and only then — the eviction hook, then a new region, then a
    /// standalone mapping. The hook sits where it does because that is the last
    /// point at which an owner could still stop the arena reserving more
    /// address space; with no hook installed, which is the default and the only
    /// configuration in this workspace, that step is one `Option::take` of a
    /// `None` and the sequence is what it was before the hook existed.
    ///
    /// **The arena's lock is held for all of this, including the hook call.**
    /// See [`JitCodeArenaEvictionHook`] for what that forbids.
    pub fn alloc(&mut self, size: usize) -> Option<ArenaBlock> {
        if size == 0 {
            // Same refusal `platform_alloc` gives, made explicit here so the
            // page rounding below never produces a zero-page block.
            return None;
        }
        let page = code_page_size();
        let rounded = size.checked_add(page - 1)? & !(page - 1);

        if self.inert || rounded > self.region_bytes {
            return self.alloc_standalone(size, rounded);
        }

        let pages = rounded / page;
        if let Some(block) = self.pop_free(pages, size) {
            self.last_alloc_mapped_region = false;
            self.last_alloc_was_standalone = false;
            return Some(block);
        }
        if let Some(block) = self.bump(rounded, size) {
            self.last_alloc_mapped_region = false;
            self.last_alloc_was_standalone = false;
            return Some(block);
        }
        // Everything the arena already holds has missed, so the next step
        // reserves more address space. This is the one moment at which an
        // owner with an eviction policy would want to be asked, so it is the
        // one moment the seam fires. Unset by default: `ask_for_eviction`
        // answers `false` without doing anything at all.
        if self.ask_for_eviction() {
            if let Some(block) = self.pop_free(pages, size) {
                self.last_alloc_mapped_region = false;
                self.last_alloc_was_standalone = false;
                return Some(block);
            }
            if let Some(block) = self.bump(rounded, size) {
                self.last_alloc_mapped_region = false;
                self.last_alloc_was_standalone = false;
                return Some(block);
            }
        }
        let from_new_region = if self.map_region().is_some() {
            self.bump(rounded, size)
        } else {
            None
        };
        if let Some(block) = from_new_region {
            self.last_alloc_mapped_region = true;
            self.last_alloc_was_standalone = false;
            return Some(block);
        }
        // Two ways to get here. Ordinarily: the OS refused a new region, and a
        // standalone mapping is the right answer because it is what the caller
        // would have got without an arena.
        //
        // The other way is reachable but has never been observed, and is worth
        // naming rather than asserting away: a region's USABLE interior is
        // `end - base`, which equals `region_bytes` only because both
        // allocators return page-aligned memory. If one ever did not, a request
        // of exactly `region_bytes` would clear the guard at the top of this
        // function, fit no region, and map a fresh one on every such
        // allocation. The result is correct — the standalone mapping below
        // serves it — but each occurrence wastes one region. `map_region` logs
        // at `info!` and the census counts `regions`, so the waste is visible
        // rather than silent.
        self.alloc_standalone(size, rounded)
    }

    /// Return a block. A region block rejoins its size class's free list; a
    /// standalone block goes back to the OS.
    ///
    /// **Read [`JitCodeArena`]'s ordering obligation before calling this.** A
    /// region block's address becomes reusable the moment this returns — or,
    /// if this free was the one that emptied a surplus region, UNMAPPED the
    /// moment this returns. Both outcomes require the caller to have purged
    /// every address-keyed registry first; the second one turns a violation
    /// into a fault instead of a silent resume, which is an improvement and
    /// not an excuse.
    pub fn free(&mut self, block: ArenaBlock) {
        match block.origin {
            BlockOrigin::Standalone => platform_free(block.ptr, block.size),
            BlockOrigin::Region(_) => {
                self.release(block.ptr as usize);
            }
        }
    }

    /// A snapshot of where every reserved byte is. See [`JitCodeArenaCensus`]
    /// for the identity it satisfies.
    ///
    /// Every quantity is DERIVED from the live map, the free lists and the
    /// regions rather than tracked incrementally, so the census cannot drift
    /// from the allocator the way a parallel counter can. The cost is O(live
    /// blocks), which is fine for something called at a report point and never
    /// on the allocation path.
    pub fn census(&self) -> JitCodeArenaCensus {
        let page = code_page_size();
        let bytes_mapped: usize = self.live_regions().map(|r| r.mapped_len).sum();
        let bytes_usable: usize = self.live_regions().map(|r| r.end - r.base).sum();
        // Computed from the mapping's own ends rather than as
        // `bytes_mapped - bytes_usable`, so that the two are independent
        // arithmetic and the census test's second identity actually checks
        // something instead of restating one subtraction twice.
        let bytes_alignment_waste: usize = self
            .live_regions()
            .map(|r| (r.base - r.mapped_base) + ((r.mapped_base + r.mapped_len) - r.end))
            .sum();
        let bytes_never_bumped: usize = self.live_regions().map(|r| r.bump_room()).sum();
        let bytes_free_listed: usize = self
            .free_by_pages
            .iter()
            .enumerate()
            .map(|(pages, list)| pages * page * list.len())
            .sum();
        let free_blocks: usize = self.free_by_pages.iter().map(Vec::len).sum();
        // The largest single request the arena could serve without mapping a
        // new region: the biggest free-listed block (a larger block can be
        // split for a smaller request) or the most room left ahead of any
        // region's bump cursor, whichever is larger.
        let largest_free_listed = self
            .free_by_pages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, list)| !list.is_empty())
            .map_or(0, |(pages, _)| pages * page);
        let largest_bump_room = self
            .live_regions()
            .map(|r| r.bump_room())
            .max()
            .unwrap_or(0);
        let largest_free_extent_bytes = largest_free_listed.max(largest_bump_room);
        let bytes_live: usize = self.live.values().map(|b| b.size).sum();
        let bytes_requested_live: usize = self.live.values().map(|b| b.requested).sum();
        JitCodeArenaCensus {
            arena_id: self.id,
            regions: self.region_count(),
            empty_regions: self.empty_region_count(),
            region_bytes: self.region_bytes,
            page_bytes: page,
            bytes_mapped,
            bytes_usable,
            bytes_alignment_waste,
            bytes_live,
            bytes_requested_live,
            bytes_padding_live: bytes_live - bytes_requested_live,
            bytes_free_listed,
            bytes_never_bumped,
            bytes_stranded: self.bytes_stranded,
            live_blocks: self.live.len(),
            free_blocks,
            largest_free_extent_bytes,
            stranded_blocks: self.stranded_blocks,
            served_from_free_list: self.served_from_free_list,
            served_from_bump: self.served_from_bump,
            served_standalone: self.served_standalone,
            region_map_failures: self.region_map_failures,
            unknown_frees: self.unknown_frees,
            blocks_split: self.blocks_split,
            regions_reclaimed: self.regions_reclaimed,
            bytes_reclaimed: self.bytes_reclaimed,
            bytes_served_total: self.bytes_served_total,
            bytes_requested_total: self.bytes_requested_total,
            bytes_fragmentation_total: self.bytes_served_total - self.bytes_requested_total,
            evictions_requested: self.evictions_requested,
            evictions_performed: self.evictions_performed,
        }
    }

    /// What the arena would tell an eviction policy if it asked right now.
    ///
    /// A deliberately small, cheap subset of [`Self::census`]: this is read on
    /// the allocation path (once per hook call) rather than at a report point,
    /// and a policy needs pressure, not a partition. `bytes_reserved` is the
    /// address space the arena has taken from the OS for regions — the number
    /// a policy is trying to keep down — and is `census().bytes_mapped`.
    pub fn pressure(&self) -> JitCodeArenaPressure {
        JitCodeArenaPressure {
            arena_id: self.id,
            bytes_reserved: self.live_regions().map(|r| r.mapped_len).sum(),
            bytes_live: self.live.values().map(|b| b.size).sum(),
            bytes_free_listed: self
                .free_by_pages
                .iter()
                .enumerate()
                .map(|(pages, list)| pages * code_page_size() * list.len())
                .sum(),
            regions: self.region_count(),
            region_bytes: self.region_bytes,
            live_blocks: self.live.len(),
            last_alloc_mapped_region: self.last_alloc_mapped_region,
            last_alloc_was_standalone: self.last_alloc_was_standalone,
        }
    }

    /// Install the eviction hook, returning whatever was there before.
    ///
    /// Read [`JitCodeArenaEvictionHook`] first: the hook is called with this
    /// arena's lock held, which rules out the obvious implementation.
    pub fn set_eviction_hook(
        &mut self,
        hook: JitCodeArenaEvictionHook,
    ) -> Option<JitCodeArenaEvictionHook> {
        self.evict.replace(hook)
    }

    /// Remove the eviction hook, returning it. The arena is then back to its
    /// default behaviour, which is what it has with no installer.
    pub fn take_eviction_hook(&mut self) -> Option<JitCodeArenaEvictionHook> {
        self.evict.take()
    }

    /// Whether a hook is installed. False everywhere in this crate today.
    #[inline]
    pub fn has_eviction_hook(&self) -> bool {
        self.evict.is_some()
    }

    // -- internals ---------------------------------------------------------

    /// The regions that are actually mapped, skipping reclaimed slots.
    fn live_regions(&self) -> impl Iterator<Item = &Region> + '_ {
        self.regions.iter().flatten()
    }

    /// Mapped regions. NOT `regions.len()`, which counts tombstones too.
    fn region_count(&self) -> usize {
        self.regions.iter().filter(|s| s.is_some()).count()
    }

    /// Mapped regions with nothing outstanding. The quantity the hysteresis
    /// rule is stated in terms of.
    fn empty_region_count(&self) -> usize {
        self.live_regions().filter(|r| r.is_reclaimable()).count()
    }

    /// Ask the owner to evict something, if an owner has asked to be asked.
    ///
    /// The hook is TAKEN for the duration of the call and put back afterwards.
    /// That is not paranoia about re-entrancy — a hook that re-entered this
    /// arena would deadlock on the mutex long before it reached this field —
    /// it is what lets the call happen at all without borrowing `self`
    /// immutably and mutably at once. A hook that panics is dropped rather
    /// than restored; the mutex is poisoned-recovered everywhere in this file,
    /// so the alternative would be an arena that keeps calling a hook that
    /// panics.
    fn ask_for_eviction(&mut self) -> bool {
        let Some(hook) = self.evict.take() else {
            return false;
        };
        self.evictions_requested += 1;
        let pressure = self.pressure();
        let evicted = hook(&pressure);
        self.evict = Some(hook);
        if evicted {
            self.evictions_performed += 1;
        }
        evicted
    }

    /// A mapping of its own, for the cases the arena cannot serve. This is the
    /// pre-arena path verbatim: one `platform_alloc`, released by one
    /// `platform_free`, with no region and no free list involved.
    fn alloc_standalone(&mut self, requested: usize, rounded: usize) -> Option<ArenaBlock> {
        // The rounded size is requested from the OS rather than the raw one:
        // both allocators round up internally anyway, so this costs nothing and
        // keeps "every block this arena hands out is page-sized" true for every
        // origin, which is what the tests get to assert without a special case.
        let ptr = platform_alloc(rounded)?;
        self.served_standalone += 1;
        self.bytes_served_total += rounded as u64;
        self.bytes_requested_total += requested as u64;
        self.last_alloc_mapped_region = false;
        self.last_alloc_was_standalone = true;
        Some(ArenaBlock {
            ptr,
            size: rounded,
            requested,
            origin: BlockOrigin::Standalone,
        })
    }

    /// Carve `rounded` bytes off the first region with room.
    ///
    /// Linear over region SLOTS, which is bounded by the high-water mark of
    /// simultaneously-mapped regions. Reclaimed slots are skipped and reused
    /// rather than removed; see [`Self::regions`].
    ///
    /// **That is the whole bound, and it used to say "itself bounded by the
    /// code-cache cap divided by the region size, 16 at the default settings".
    /// Nothing enforces that.** No code in `JitCodeArena`, [`Self::map_region`],
    /// [`Self::alloc`] or `ExecutableBuffer::new_in` reads
    /// `jit_code_cache_cap_bytes()`; the only occurrences of the cap in this
    /// file are prose. `map_region` calls `platform_alloc(self.region_bytes)`
    /// unconditionally, with no region count and no byte budget, and fails only
    /// when the OS refuses.
    ///
    /// The cap is enforced one layer up, in `jit_code_cache_at_capacity` /
    /// `jit_code_cache_under_pressure`, against `COMMITTED_JIT_CODE_BYTES` — and
    /// `new_in` adds only a BLOCK's page-rounded size to that counter, never the
    /// region's 16 MiB. So reserved region bytes are outside the capped quantity
    /// altogether, and the arithmetic does not hold even indirectly: blocks are
    /// never coalesced, the hysteresis rule keeps a spare
    /// ([`JIT_CODE_ARENA_EMPTY_REGION_SPARES`]), and
    /// `CRATONVM_JIT_CODE_CACHE_MAX_MB=0` sets the cap to `usize::MAX`, which
    /// makes "divided by the region size" meaningless.
    ///
    /// The scan is still cheap in practice — but on the observed high-water mark
    /// of live regions, which is a measurement nobody has taken, not on a
    /// constant this module guarantees. `JIT_CODE_DEFAULT_REGION_BYTES`' reason
    /// (c) made the same claim and now says the same thing.
    fn bump(&mut self, rounded: usize, requested: usize) -> Option<ArenaBlock> {
        // One pass that yields the `&mut Region` directly, rather than a
        // `position` followed by an `expect` that re-asserts what `position`
        // just proved. The assertion was sound; it was also a panic on a
        // compile thread, where a panic is a PERMANENT decline for the method
        // (`tiered::contain_compile_panic` catches it and
        // `CompileOutcome::declined` is the result). `jit/tests/panic_free_compile_ratchet.rs`
        // counts these for exactly that reason.
        let (idx, region) = self
            .regions
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.as_ref().is_some_and(|r| r.bump_room() >= rounded))
            .and_then(|(i, slot)| slot.as_mut().map(|r| (i, r)))?;
        let addr = region.cursor;
        region.cursor += rounded;
        self.served_from_bump += 1;
        Some(self.claim(addr, rounded, requested, idx))
    }

    fn claim(
        &mut self,
        addr: usize,
        rounded: usize,
        requested: usize,
        region: usize,
    ) -> ArenaBlock {
        if let Some(r) = self.regions.get_mut(region).and_then(Option::as_mut) {
            r.live_blocks += 1;
        } else {
            debug_assert!(false, "claim from reclaimed region slot {region}");
        }
        self.bytes_served_total += rounded as u64;
        self.bytes_requested_total += requested as u64;
        self.live.insert(
            addr,
            LiveBlock {
                size: rounded,
                requested,
                region,
            },
        );
        ArenaBlock {
            // Cast: an address inside a region this arena mapped and still
            // holds; the arena will not hand it out again until it is released.
            ptr: addr as *mut u8,
            size: rounded,
            requested,
            origin: BlockOrigin::Region(region),
        }
    }

    /// Map one more region. `None` means the OS refused, which is a fallback
    /// condition and not an error: the caller then takes a standalone mapping,
    /// which is what it would have done without an arena at all.
    fn map_region(&mut self) -> Option<usize> {
        let page = code_page_size();
        let Some(mapped) = platform_alloc(self.region_bytes) else {
            self.region_map_failures += 1;
            tracing::warn!(
                region_bytes = self.region_bytes,
                regions = self.region_count(),
                "jit code arena: the OS refused a new code region; this compile and \
                 any later one that does not fit an existing region falls back to a \
                 standalone mapping. That is the pre-arena behaviour, so code still \
                 compiles — but the address-space saving is gone for those methods."
            );
            return None;
        };
        let mapped_base = mapped as usize;
        let base = mapped_base.saturating_add(page - 1) & !(page - 1);
        let end = mapped_base.saturating_add(self.region_bytes) & !(page - 1);
        if end <= base || end - base < page {
            // Cannot happen with a page-aligned allocator and a region size of
            // at least one page, but if it ever did, a region with no usable
            // interior would sit in the list forever failing every `bump`.
            platform_free(mapped, self.region_bytes);
            self.region_map_failures += 1;
            tracing::warn!(
                mapped_base = mapped_base,
                region_bytes = self.region_bytes,
                page = page,
                "jit code arena: a freshly mapped region has no page-aligned \
                 interior; releasing it and falling back to standalone mappings"
            );
            return None;
        }
        let region = Region {
            mapped_base,
            mapped_len: self.region_bytes,
            base,
            cursor: base,
            end,
            live_blocks: 0,
            free_blocks: 0,
            free_bytes: 0,
            stranded_bytes: 0,
        };
        // A slot left behind by `reclaim_region` is reused rather than appended
        // to, so `regions.len()` tracks the high-water mark of simultaneously
        // mapped regions instead of the number ever mapped. Reuse is sound
        // because reclamation only happens to a slot nothing refers to; see
        // [`Self::regions`].
        let idx = match self.regions.iter().position(Option::is_none) {
            Some(idx) => {
                self.regions[idx] = Some(region);
                idx
            }
            None => {
                self.regions.push(Some(region));
                self.regions.len() - 1
            }
        };
        tracing::info!(
            region = idx,
            base = base,
            end = end,
            usable = end - base,
            page = page,
            regions = self.region_count(),
            "jit code arena: mapped a code region. Every method compiled into it \
             pays the OS allocation granularity once, here, instead of once per \
             method."
        );
        Some(idx)
    }

    /// Move a live region block onto its size class's free list.
    ///
    /// Returns `false` for an address this arena has no live block at, which
    /// means either a double free or a pointer from somewhere else; both are
    /// counted and warned about rather than acted on, because the one thing
    /// that must not happen is putting an unknown address on a free list and
    /// handing it to a compiler as writable memory.
    fn release(&mut self, addr: usize) -> bool {
        // The half of the ordering obligation this module can check: by the
        // time a block comes back, `ExecutableBuffer::drop` must already have
        // deregistered its range, or a `validate_code_ptr` could still accept a
        // pointer into memory the next compile is about to overwrite. Debug
        // only: this takes a global lock, and the release path runs on compile
        // threads and during teardown.
        debug_assert!(
            !crate::jit_code_regions()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(addr as *const u8),
            "arena block {addr:#x} returned while still registered in \
             jit_code_regions; see JitCodeArena's ordering obligation"
        );
        let Some(b) = self.live.remove(&addr) else {
            self.unknown_frees += 1;
            tracing::warn!(
                addr = addr,
                "jit code arena: asked to free {addr:#x}, which lies inside one of \
                 this arena's regions but is not a live block. That is a double free \
                 or a pointer into the middle of a block; the address is NOT put on \
                 the free list, so it is leaked rather than handed to a later \
                 compile."
            );
            return false;
        };
        let page = code_page_size();
        let pages = b.size / page;
        let region = b.region;
        if let Some(r) = self.regions.get_mut(region).and_then(Option::as_mut) {
            r.live_blocks -= 1;
        } else {
            debug_assert!(false, "released a block of reclaimed region slot {region}");
        }

        // Restore the block to WRITABLE before it can be recycled.
        //
        // This is not bookkeeping, it is the difference between a working
        // arena and one that faults on the first reuse. `ExecutableBuffer`'s
        // lifecycle is `new` (RW) -> write -> `finalize` (RX) -> ... -> `drop`,
        // and `drop` does NOT call `make_writable`: it had no reason to, because
        // before the arena the mapping went straight back to the OS and the
        // next buffer got fresh RW pages. A block recycled off a free list is
        // not fresh — it is still RX from the `finalize` of the method that
        // just died — so the first `emit` into it would take an access
        // violation on Windows and a SIGSEGV on Linux. One `VirtualProtect` /
        // `mprotect` per retirement, on the retirement path and not the
        // allocation path.
        //
        // It also removes EXECUTE from the block, which is a strict improvement
        // on both of the things it replaces. Against the pre-arena path, it is
        // gentler: that path called `platform_free`, and a thread still
        // executing in the block faulted on an unmapped page rather than on a
        // non-executable one — same fault, less destruction, and the question
        // of whether a thread can still be in there is the retirement queue's
        // (`reclaim_is_authorised`), not this function's. Against the arena's
        // first shape it is better still: a freed block used to sit on the free
        // list STILL EXECUTABLE, so a stale jump into a retired body ran
        // retired instructions silently. Now it faults.
        //
        // If the flip fails the block is STRANDED: not live, not free, and
        // permanently accounted against its region so that the region can never
        // be reclaimed while a piece of it has unknown protection. Leaking it is
        // the only safe answer — handing a compiler a block that may not be
        // writable is the fault this code exists to avoid, and unmapping the
        // region around it would be worse still.
        if let Err(e) = make_writable(addr as *mut u8, b.size) {
            self.stranded_blocks += 1;
            self.bytes_stranded += b.size;
            if let Some(r) = self.regions.get_mut(region).and_then(Option::as_mut) {
                r.stranded_bytes += b.size;
            }
            tracing::warn!(
                addr = addr,
                size = b.size,
                error = %e,
                "jit code arena: could not restore block {addr:#x} to writable after \
                 its method was retired, so it is leaked rather than recycled — a \
                 later compile would fault writing into an RX block. Its region is \
                 now pinned and can never be reclaimed."
            );
            return true;
        }

        self.push_free(addr, region, pages);
        self.maybe_reclaim_regions();
        true
    }

    /// Put `[addr, addr + pages*page)` on its size class's free list.
    ///
    /// The single place `Region::free_blocks` and `Region::free_bytes` go up,
    /// which is what makes them trustworthy enough for
    /// [`Region::is_reclaimable`] to decide an unmapping on.
    ///
    /// A block whose region slot no longer holds a region is DROPPED rather
    /// than pushed. That cannot happen — a region is reclaimed only when it has
    /// no live block and no free entry, so nothing can come back to it
    /// afterwards — but the consequence if it ever did is an address on a free
    /// list pointing into unmapped memory, which the next compile of that size
    /// class would be handed as somewhere to write instructions. Leaking is the
    /// cheap side of that trade; the `debug_assert` makes the case loud in a
    /// debug build rather than only visible as a census anomaly.
    fn push_free(&mut self, addr: usize, region: usize, pages: usize) {
        let bytes = pages * code_page_size();
        let Some(r) = self.regions.get_mut(region).and_then(Option::as_mut) else {
            debug_assert!(false, "free entry for reclaimed region slot {region}");
            self.unknown_frees += 1;
            tracing::warn!(
                addr = addr,
                region = region,
                "jit code arena: block {addr:#x} belongs to a region slot that no \
                 longer holds a region, so it is leaked rather than put on a free \
                 list. A free entry naming an unmapped region would be handed to the \
                 next compile of its size class."
            );
            return;
        };
        r.free_blocks += 1;
        r.free_bytes += bytes;
        self.free_list(pages).push((addr, region));
    }

    /// Take `pages` pages off the free lists, splitting a larger block if the
    /// exact class is empty.
    ///
    /// Exact class first, because that is the overwhelmingly common case (JIT
    /// body sizes cluster) and it is a `pop` off a `Vec`. Only on a miss does
    /// this scan upward for the smallest class that can be split; the scan is
    /// bounded by `free_by_pages.len()`, which is one past the largest class
    /// ever freed and is single digits for a method-body arena.
    ///
    /// The split's own correctness argument — page alignment, page sizing, and
    /// why the two pieces can never both be outstanding — is in
    /// [`JitCodeArena`]'s "The free list splits, and does not coalesce".
    ///
    /// The loop is for one case that cannot arise: an entry naming a region
    /// slot that no longer holds a region. [`Self::reclaim_region`] removes
    /// every entry it owns before it unmaps, so no such entry can exist; if one
    /// ever did, handing it out would be a compile writing into unmapped
    /// memory. Discarding it and continuing costs one iteration in a case that
    /// is already a bug and cannot cost anything in a case that is not.
    fn pop_free(&mut self, pages: usize, requested: usize) -> Option<ArenaBlock> {
        let page = code_page_size();
        loop {
            let class = self
                .free_by_pages
                .iter()
                .enumerate()
                .skip(pages)
                .find(|(_, list)| !list.is_empty())
                .map(|(class, _)| class)?;
            // `let ... else` rather than an `expect` on what `find` just
            // proved non-empty: same answer, no panic. See `bump` for why a
            // panic here is a permanent decline rather than a bailout.
            let Some((addr, region)) = self.free_by_pages[class].pop() else {
                return None;
            };
            // The whole entry leaves the free list before either piece exists,
            // so the region's free counters are decremented by the WHOLE class
            // and the remainder is then pushed back as a new entry. Adjusting
            // by the difference instead would be the same arithmetic with one
            // more place for a split to drift it.
            let Some(r) = self.regions.get_mut(region).and_then(Option::as_mut) else {
                debug_assert!(false, "free entry survived reclamation of region {region}");
                tracing::warn!(
                    addr = addr,
                    region = region,
                    "jit code arena: free entry {addr:#x} named region slot {region}, \
                     which holds no region. Discarded rather than served — it would \
                     have been a compile writing into unmapped memory."
                );
                continue;
            };
            r.free_blocks -= 1;
            r.free_bytes -= class * page;
            if class > pages {
                self.push_free(addr + pages * page, region, class - pages);
                self.blocks_split += 1;
            }
            self.served_from_free_list += 1;
            return Some(self.claim(addr, pages * page, requested, region));
        }
    }

    fn free_list(&mut self, pages: usize) -> &mut Vec<(usize, usize)> {
        if self.free_by_pages.len() <= pages {
            self.free_by_pages.resize_with(pages + 1, Vec::new);
        }
        &mut self.free_by_pages[pages]
    }

    /// Apply the hysteresis rule: if more than
    /// [`JIT_CODE_ARENA_EMPTY_REGION_SPARES`] regions are empty, unmap the
    /// surplus.
    ///
    /// Called from [`Self::release`] only — the only moment a region can BECOME
    /// empty — and O(region slots), which is single digits.
    ///
    /// The spare kept is the empty region with the most contiguous bump room,
    /// ties to the lowest slot. Bump room, and not free-listed bytes, because
    /// bump room serves a request of any size up to itself while a free list
    /// only serves the classes it holds: keeping the region that can say yes to
    /// the most future requests is what makes the spare worth its address space.
    /// See [`JitCodeArena`]'s hysteresis section for why a spare is kept at all.
    fn maybe_reclaim_regions(&mut self) {
        let empty: Vec<usize> = self
            .regions
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.as_ref().is_some_and(Region::is_reclaimable))
            .map(|(idx, _)| idx)
            .collect();
        if empty.len() <= JIT_CODE_ARENA_EMPTY_REGION_SPARES {
            return;
        }
        // Sort the candidates by "worth keeping" descending, then reclaim past
        // the spare count. The key is (bump room, lowest slot), so the ordering
        // is total and the choice is deterministic run to run — which is what
        // lets a test assert WHICH region survived rather than only how many.
        let mut ranked = empty;
        ranked.sort_by_key(|&idx| {
            // `map(..).unwrap_or(0)` rather than `expect`: `empty` was
            // filtered to occupied slots, so the fallback is unreachable — and
            // an unreachable `expect` is still a panic in a codegen path. A `0`
            // key sorts such a slot last, which is also the right answer for a
            // region that is not there.
            let room = self.regions[idx]
                .as_ref()
                .map(|r| r.bump_room())
                .unwrap_or(0);
            (std::cmp::Reverse(room), idx)
        });
        for &idx in ranked.iter().skip(JIT_CODE_ARENA_EMPTY_REGION_SPARES) {
            self.reclaim_region(idx);
        }
    }

    /// Unmap region `idx` and tombstone its slot.
    ///
    /// # The order is the whole of the safety argument
    ///
    /// 1. Take the region out of its slot, so nothing can bump into it while
    ///    the rest of this runs.
    /// 2. Remove EVERY free-list entry that names it. No free entry may survive
    ///    its region: a surviving entry is an address on a free list that the
    ///    next allocation of that size class would hand to a compiler, pointing
    ///    at memory this function is about to unmap.
    /// 3. Withdraw the region's span from `ARENA_SPANS`, and unmap while still
    ///    holding that lock. The span must go BEFORE the memory, not after: a
    ///    span that outlives its mapping would make [`free_if_arena`] claim a
    ///    later, unrelated mapping that the OS happened to place at the same
    ///    address, swallow its free and leak it. Holding the spans lock across
    ///    the `platform_free` closes the window entirely, and cannot deadlock —
    ///    the lock order in this file is arena-then-spans (see
    ///    [`publish_arena_spans`]), `platform_free` takes neither, and
    ///    `free_if_arena` never holds spans while it waits for an arena.
    ///
    /// The caller must already have established [`Region::is_reclaimable`].
    fn reclaim_region(&mut self, idx: usize) {
        let Some(region) = self.regions.get_mut(idx).and_then(Option::take) else {
            debug_assert!(false, "reclaim of an already-empty slot {idx}");
            return;
        };
        debug_assert!(
            region.live_blocks == 0 && region.stranded_bytes == 0,
            "reclaiming region {idx} with {} live and {} stranded bytes",
            region.live_blocks,
            region.stranded_bytes
        );

        let mut removed = 0usize;
        for list in self.free_by_pages.iter_mut() {
            let before = list.len();
            list.retain(|&(_, owner)| owner != idx);
            removed += before - list.len();
        }
        debug_assert_eq!(
            removed, region.free_blocks,
            "region {idx} claimed {} free blocks but {removed} entries named it",
            region.free_blocks
        );

        let owner_id = self.id;
        {
            let mut spans = ARENA_SPANS.lock().unwrap_or_else(|e| e.into_inner());
            spans
                .retain(|s| !(s.owner_id == owner_id && s.lo == region.base && s.hi == region.end));
            ARENA_SPAN_COUNT.store(spans.len(), std::sync::atomic::Ordering::Release);
            // Cast: `mapped_base` is exactly the pointer `platform_alloc`
            // returned for this region, which is what both `VirtualFree(.., 0,
            // MEM_RELEASE)` and `munmap` require — never an interior address.
            platform_free(region.mapped_base as *mut u8, region.mapped_len);
        }

        self.regions_reclaimed += 1;
        self.bytes_reclaimed += region.mapped_len as u64;
        tracing::info!(
            region = idx,
            base = region.base,
            end = region.end,
            mapped_len = region.mapped_len,
            free_entries_dropped = removed,
            regions = self.region_count(),
            empty_regions = self.empty_region_count(),
            "jit code arena: reclaimed an empty code region. The mapping is \
             returned to the OS — unless CRATONVM_JIT_POISON_FREE is set, which \
             retires it as PROT_NONE instead and keeps the address space. One \
             empty region is kept as a spare, so this only happens once a second \
             region has gone empty."
        );
    }
}

/// Where every byte a [`JitCodeArena`] reserved currently is.
///
/// # The identity
///
/// ```text
/// bytes_requested_live + bytes_padding_live + bytes_free_listed
///   + bytes_never_bumped + bytes_stranded    ==  bytes_usable
/// bytes_usable + bytes_alignment_waste       ==  bytes_mapped
/// ```
///
/// That is a full partition of the region space: every byte is in a live
/// block's useful part, in a live block's page-rounding padding, on a free
/// list, still ahead of a bump cursor, or stranded (see
/// [`JitCodeArena::release`], where a block that cannot be made writable again
/// is leaked rather than recycled — zero on every run that has not had an
/// `mprotect` fail). Asserted by `the_census_partitions_every_reserved_byte`,
/// because a census whose parts do not sum is a census that will be used to
/// argue for a change and be wrong.
///
/// The identity is over the regions CURRENTLY mapped. A reclaimed region leaves
/// it entirely — its mapping is gone, its free entries went with it — and the
/// only trace it leaves is [`Self::regions_reclaimed`] and
/// [`Self::bytes_reclaimed`]. So `bytes_mapped` can now go DOWN between two
/// censuses, which it could not before.
///
/// Standalone blocks are also outside the identity: they are not part of any
/// region, they are released to the OS, and the only trace they leave is
/// [`Self::served_standalone`].
///
/// # The cumulative counters are not part of the identity
///
/// [`Self::bytes_served_total`], [`Self::bytes_requested_total`] and their
/// difference [`Self::bytes_fragmentation_total`] are monotonic totals over the
/// whole life of the arena and count BOTH origins, so they do not relate to the
/// partition above at all. They answer a different question — "what has page
/// rounding cost in total?" — and are the counters a decision about the page
/// class would be made from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitCodeArenaCensus {
    /// Which arena this is, so two arenas' lines can be told apart in a log.
    pub arena_id: u64,
    /// Regions currently mapped. DECREASES when a region is reclaimed; see
    /// [`Self::regions_reclaimed`].
    pub regions: usize,
    /// Of those, the ones with nothing outstanding — no live block, no stranded
    /// block, everything they handed out back on a free list. The hysteresis
    /// rule keeps at most [`JIT_CODE_ARENA_EMPTY_REGION_SPARES`] of these
    /// between allocations, so a larger value here means a `release` is in
    /// flight or the arena has not had a free since they emptied.
    pub empty_regions: usize,
    /// Bytes per region for this arena.
    pub region_bytes: usize,
    /// The page size blocks are rounded to; see [`code_page_size`].
    pub page_bytes: usize,
    /// Bytes obtained from the OS for regions.
    pub bytes_mapped: usize,
    /// Bytes of that which are inside a region's page-aligned interior.
    pub bytes_usable: usize,
    /// Head and tail bytes lost to page-aligning a mapping. Zero on every
    /// platform this builds for, since both allocators return page-aligned
    /// memory; reported so that "zero" is a measurement rather than a belief.
    pub bytes_alignment_waste: usize,
    /// Page-rounded bytes currently handed out of regions.
    pub bytes_live: usize,
    /// Of those, the bytes callers actually asked for.
    pub bytes_requested_live: usize,
    /// Of those, the page-rounding padding. Usable by the owning buffer (see
    /// `ExecutableBuffer::new_in`), but not asked for.
    pub bytes_padding_live: usize,
    /// Page-rounded bytes sitting on the size-class free lists.
    pub bytes_free_listed: usize,
    /// Bytes still ahead of a bump cursor, never yet handed out.
    pub bytes_never_bumped: usize,
    /// Bytes of returned blocks that could not be made writable again and are
    /// therefore neither live nor free. Zero unless an `mprotect` /
    /// `VirtualProtect` has failed; see [`JitCodeArena::release`].
    pub bytes_stranded: usize,
    /// Live region blocks.
    pub live_blocks: usize,
    /// Entries across all the size-class free lists. Against
    /// [`Self::bytes_free_listed`] this gives the mean free-block size, which
    /// is the fragmentation measurement the no-coalescing decision would be
    /// revisited on; see [`JitCodeArena`]'s free-list section.
    pub free_blocks: usize,
    /// The largest single allocation the arena could serve from what it
    /// already holds: the biggest free-listed block or the most bump room left
    /// in any one region, whichever is larger (round 9 wave 2). The free lists
    /// never coalesce, so this — not `bytes_free_listed + bytes_never_bumped`
    /// — is the "largest free extent" a fragmentation ratio needs; the VM's
    /// `code_cache_lifecycle_raw` free-space gauge can read it
    /// (`NOTES-vmside.md` declined request 3 for want of exactly this).
    pub largest_free_extent_bytes: usize,
    /// Blocks counted in [`Self::bytes_stranded`].
    pub stranded_blocks: usize,
    /// Allocations served by recycling a freed block. This is the number the
    /// pooling half of the change exists to make non-zero. Includes the ones
    /// that needed a split; see [`Self::blocks_split`].
    pub served_from_free_list: u64,
    /// Allocations served by advancing a bump cursor.
    pub served_from_bump: u64,
    /// Allocations that got their own mapping instead — inert platform,
    /// over-region-size request, or a region the OS refused. Each one is a
    /// method that still pays the full allocation-granularity tax.
    pub served_standalone: u64,
    /// Regions the OS refused.
    pub region_map_failures: u64,
    /// Frees this arena could not match to a live block. **TWO causes, and they
    /// want different investigations** — this field used to name only the first,
    /// which sent a reader of the second to the wrong bug.
    ///
    /// 1. [`JitCodeArena::release`]: the address is inside a region but is not a
    ///    live block. That is a double free or an interior pointer, and the
    ///    block's bytes are leaked rather than recycled. The caller is wrong.
    /// 2. `JitCodeArena::push_free`: a returned block names a region SLOT that
    ///    no longer holds a region. That is the arena's own bookkeeping, not the
    ///    caller's, and this module's doc calls it the more dangerous of the two
    ///    — it is the shape in which "an address on a free list points into
    ///    unmapped memory" would arise.
    ///
    /// Both increment this one counter, so a non-zero value does not say which.
    /// The `warn!` each site emits does, and it is the thing to read first:
    /// `log_jit_code_arena_census`'s own warn for this field describes only
    /// cause 1. Read the per-site warnings, not this number, to decide which
    /// happened.
    pub unknown_frees: u64,
    /// Allocations served by splitting a larger free block, pushing the
    /// remainder into its own class. A subset of [`Self::served_from_free_list`]
    /// and also the rate at which the free lists shatter, since nothing
    /// coalesces them back.
    pub blocks_split: u64,
    /// Regions unmapped because they went empty. See
    /// [`JIT_CODE_ARENA_EMPTY_REGION_SPARES`] for why this is not simply "every
    /// region that ever went empty".
    pub regions_reclaimed: u64,
    /// Mapped bytes handed back by those reclamations.
    ///
    /// Address space RETURNED on Windows and on Unix without
    /// `CRATONVM_JIT_POISON_FREE`. With that flag set, `platform_free` is
    /// `mprotect(PROT_NONE)` and the address space is not returned at all — so
    /// this counts bytes retired, not bytes given back, and the two differ in
    /// exactly that one configuration.
    pub bytes_reclaimed: u64,
    /// Page-rounded bytes ever handed out, over the whole life of the arena and
    /// across both origins.
    pub bytes_served_total: u64,
    /// Bytes callers ever asked for, over the same population.
    pub bytes_requested_total: u64,
    /// `bytes_served_total - bytes_requested_total`: the cumulative cost of
    /// page rounding, i.e. bytes reserved for a method that the method did not
    /// ask for.
    ///
    /// This is internal fragmentation and nothing else. It is NOT the external
    /// fragmentation that the missing coalescing causes — that one has no
    /// single number, and the proxy for it is
    /// `bytes_free_listed / free_blocks` falling towards one page while
    /// `bytes_free_listed` stays large.
    pub bytes_fragmentation_total: u64,
    /// Times the eviction hook was called, i.e. times the arena was about to
    /// reserve more address space and an owner had asked to be told.
    /// Zero whenever no hook is installed, which is everywhere today.
    pub evictions_requested: u64,
    /// Of those, the calls in which the hook answered "I freed something".
    ///
    /// A hook that merely QUEUES a victim (which is all a hook can do while the
    /// arena lock is held — see [`JitCodeArenaEvictionHook`]) and answers
    /// `true` is counted here even though this allocation still had to map a
    /// region. `evictions_performed` against `regions` over a run is what would
    /// say whether the seam ever prevented a growth.
    pub evictions_performed: u64,
}

/// A shareable arena. `Arc<Mutex<..>>` and not `&mut`, for two reasons that are
/// both about the free path rather than the allocation path.
///
/// Allocation happens on compile threads, of which there are several, so the
/// arena needs interior mutability and a lock — that much is ordinary. The
/// `Arc` is what lets [`free_if_arena`] name the owning arena from a `Weak` in
/// the span table without keeping it alive: a block can, in principle, outlive
/// every handle, and a `Weak` that fails to upgrade is how that is detected and
/// turned into a leak instead of a wild `munmap`.
pub type JitArenaHandle = std::sync::Arc<std::sync::Mutex<JitCodeArena>>;

/// The process-wide arena `ExecutableBuffer::new_in` uses when no other is
/// named. Never dropped, which is what makes [`JitCodeArena`]'s "a block can
/// outlive its arena handle, so the regions are leaked in that case" a
/// non-issue in practice — the handle is static, so the case never arises and
/// region reclamation is the only thing that ever unmaps.
pub fn shared_jit_code_arena() -> &'static JitArenaHandle {
    static ARENA: std::sync::OnceLock<JitArenaHandle> = std::sync::OnceLock::new();
    ARENA.get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(JitCodeArena::new())))
}

/// The census of [`shared_jit_code_arena`].
pub fn jit_code_arena_census() -> JitCodeArenaCensus {
    shared_jit_code_arena()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .census()
}

/// Emit the arena census at `info!`, and each pathology at `warn!`.
///
/// Deliberately NOT `debug!`/`trace!`. The workspace `Cargo.toml` pins
/// `tracing` with `release_max_level_info`, so those two macros expand to
/// no-ops in a release build and cannot be recovered with any `RUST_LOG` value
/// — which would make this census unreadable in exactly the runs that would
/// justify turning the arena on. The zeros matter too: `served_from_free_list =
/// 0` says the pooling never engaged, and `served_standalone = 0` says nothing
/// fell back, and neither is visible if the line is compiled away.
pub fn log_jit_code_arena_census(census: &JitCodeArenaCensus) {
    tracing::info!(
        arena_id = census.arena_id,
        regions = census.regions,
        empty_regions = census.empty_regions,
        region_bytes = census.region_bytes,
        page_bytes = census.page_bytes,
        bytes_mapped = census.bytes_mapped,
        // `bytes_usable` and `bytes_alignment_waste` were computed by every
        // `census()` and logged by nothing, while `bytes_alignment_waste`'s own
        // doc says it is "reported so that 'zero' is a measurement rather than a
        // belief". It was not reported, so the zero was a belief — which is this
        // round's defect class exactly, and in the one function whose job is to
        // make the census readable. They are cheap: two `usize` fields already
        // in the struct.
        //
        // Read them as a pair against `bytes_mapped`:
        // `bytes_usable + bytes_alignment_waste == bytes_mapped` is the
        // partition `the_census_partitions_every_reserved_byte` asserts, so a
        // non-zero waste on a platform this builds for would mean an allocator
        // stopped returning page-aligned memory — which is the assumption the
        // whole W^X argument for page-sized blocks rests on.
        bytes_usable = census.bytes_usable,
        bytes_alignment_waste = census.bytes_alignment_waste,
        // The fragmentation numerator. `bytes_free_listed + bytes_never_bumped`
        // is NOT a substitute (the free lists never coalesce), which is why the
        // field exists; it reached `vm/src/jit/code_cache_lifecycle.rs`'s
        // free-space gauge and not this line, so a reader of the census alone
        // could not compute the ratio the two shape events below discuss.
        largest_free_extent_bytes = census.largest_free_extent_bytes,
        bytes_live = census.bytes_live,
        bytes_requested_live = census.bytes_requested_live,
        bytes_padding_live = census.bytes_padding_live,
        bytes_free_listed = census.bytes_free_listed,
        bytes_never_bumped = census.bytes_never_bumped,
        live_blocks = census.live_blocks,
        free_blocks = census.free_blocks,
        served_from_free_list = census.served_from_free_list,
        served_from_bump = census.served_from_bump,
        served_standalone = census.served_standalone,
        blocks_split = census.blocks_split,
        regions_reclaimed = census.regions_reclaimed,
        bytes_reclaimed = census.bytes_reclaimed,
        bytes_served_total = census.bytes_served_total,
        bytes_requested_total = census.bytes_requested_total,
        bytes_fragmentation_total = census.bytes_fragmentation_total,
        evictions_requested = census.evictions_requested,
        evictions_performed = census.evictions_performed,
        "jit code arena census"
    );
    // The two shape questions the census is for, spelled out rather than left
    // to whoever reads the line. Both are `info!` and unconditional: a zero is
    // the answer just as often as a non-zero is, and "splitting never engaged"
    // is exactly as worth knowing as "splitting engaged constantly".
    tracing::info!(
        blocks_split = census.blocks_split,
        served_from_free_list = census.served_from_free_list,
        free_blocks = census.free_blocks,
        bytes_free_listed = census.bytes_free_listed,
        mean_free_block_bytes = if census.free_blocks == 0 {
            0
        } else {
            census.bytes_free_listed / census.free_blocks
        },
        page_bytes = census.page_bytes,
        "jit code arena: free-list shape. The arena splits and does NOT coalesce, \
         so a mean free-block size decaying towards one page while the free-listed \
         bytes stay large is the fragmentation that would justify building \
         coalescing; a mean that holds is not."
    );
    tracing::info!(
        regions_reclaimed = census.regions_reclaimed,
        bytes_reclaimed = census.bytes_reclaimed,
        regions = census.regions,
        empty_regions = census.empty_regions,
        spares_kept = JIT_CODE_ARENA_EMPTY_REGION_SPARES,
        "jit code arena: region reclamation. A region is unmapped only once a \
         SECOND region has gone empty, so a steady-state arena keeps one empty \
         region by design and `regions_reclaimed = 0` on a run that never had two."
    );
    if census.evictions_requested != 0 {
        tracing::info!(
            evictions_requested = census.evictions_requested,
            evictions_performed = census.evictions_performed,
            regions = census.regions,
            "jit code arena: the eviction hook was asked {} time(s) and answered \
             'evicted' {} time(s). A hook can only QUEUE a victim while the arena \
             lock is held, so a 'performed' does not mean the allocation that \
             asked avoided a new region.",
            census.evictions_requested,
            census.evictions_performed
        );
    }
    if census.stranded_blocks != 0 {
        tracing::warn!(
            stranded_blocks = census.stranded_blocks,
            bytes_stranded = census.bytes_stranded,
            "jit code arena: {} returned block(s) could not be restored to writable \
             and were leaked rather than recycled. Their regions are pinned and can \
             never be reclaimed. A non-zero value here means a VirtualProtect or \
             mprotect on memory this process owns is failing, which is not a \
             condition this module can work around.",
            census.stranded_blocks
        );
    }
    if census.served_standalone != 0 {
        tracing::warn!(
            served_standalone = census.served_standalone,
            region_map_failures = census.region_map_failures,
            inert = JIT_CODE_ARENA_IS_INERT,
            "jit code arena: {} allocation(s) took a standalone mapping and so still \
             pay the full OS allocation granularity. Expected on macOS/ARM64 (the \
             arena is inert there) and for requests larger than one region; anything \
             else means regions could not be mapped.",
            census.served_standalone
        );
    }
    if census.unknown_frees != 0 {
        tracing::warn!(
            unknown_frees = census.unknown_frees,
            "jit code arena: {} free(s) named an address inside a region that was not \
             a live block. Each one leaked its block rather than risking handing a \
             stale address to a later compile; the caller is double-freeing or \
             passing an interior pointer.",
            census.unknown_frees
        );
    }
}

/// `CRATONVM_JIT_CODE_ARENA=1` — route `ExecutableBuffer::new_in` through the
/// code arena. **Default OFF.**
///
/// Declared in `cratonvm_types::flag_groups::INVENTORY` (token `code-arena`,
/// group `JIT`), so `CRATONVM_JIT=code-arena` turns it on too:
/// [`cratonvm_types::flags::runtime_flag_on`] routes a declared name through
/// the grouped resolver rather than straight to the environment.
///
/// Off because nothing in this change was measured. The address-space figure
/// the arena exists to recover is the round-8 TODO's arithmetic, this session
/// ran no workload, and this project does not move a default without evidence.
/// With the flag off, `new_in` is `new` — the same call, the same mapping, the
/// same accounting — so the arena costs a branch on a path that runs once per
/// compiled method.
///
/// Always off where the arena is inert, so a run on macOS/ARM64 that sets the
/// flag gets today's behaviour rather than a differently-shaped fallback.
pub fn jit_code_arena_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        if JIT_CODE_ARENA_IS_INERT {
            return false;
        }
        cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_CODE_ARENA")
    })
}

// --- address -> owning arena, for free-on-drop -----------------------------

/// One region's address range and who owns it.
struct ArenaSpan {
    lo: usize,
    hi: usize,
    /// [`JitCodeArena::id`] of the owner — never recycled, so a span left
    /// behind by a dead arena can never be mistaken for a live one's.
    owner_id: u64,
    owner: std::sync::Weak<std::sync::Mutex<JitCodeArena>>,
}

/// Hands out [`JitCodeArena::id`]. Monotonic for the life of the process; `0`
/// is never used, so a zero in a dump means "uninitialised", not "the first
/// arena".
static NEXT_ARENA_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

static ARENA_SPANS: std::sync::Mutex<Vec<ArenaSpan>> = std::sync::Mutex::new(Vec::new());

/// `ARENA_SPANS.len()`, readable without the lock so that the overwhelmingly
/// common case — no arena has ever mapped a region, because the flag is off —
/// costs one atomic load on the free path instead of a mutex.
///
/// # Why a zero here cannot be stale in the direction that matters
///
/// The dangerous outcome would be a thread reading zero, skipping the span
/// lookup, and sending an arena block's interior address to `platform_free`.
/// It cannot happen. A block's address only becomes freeable after
/// `ExecutableBuffer::new_in` returned it, and `new_in` runs
/// [`publish_arena_spans`] — which stores this counter with `Release` — before
/// it returns, under the arena lock that the allocation itself held. Every
/// thread that can name the address therefore has a happens-before edge to
/// that store, and the `Acquire` load in [`free_if_arena`] observes it. The
/// ordering pair is belt-and-braces over the mutex, which already supplies the
/// edge; it is spelled out because the counter is the one part of this table
/// read without the mutex.
static ARENA_SPAN_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Make `arena`'s regions findable by address.
///
/// Called with the arena's own lock held, from `ExecutableBuffer::new_in`,
/// after every allocation — the early return makes it a length comparison in
/// the case where no new region was mapped, which is all but one allocation in
/// several thousand.
///
/// # Lock order
///
/// This takes `ARENA_SPANS` while holding the arena lock, and so does
/// [`JitCodeArena::reclaim_region`]. [`free_if_arena`] takes them in the
/// opposite order in source order but never HOLDS both: it copies the `Weak`
/// out and drops the spans guard before locking the arena. So there is no
/// cycle, and that is a property of how `free_if_arena` is written, not a
/// coincidence — do not hoist its arena lock into the spans scope.
///
/// # Why the count comparison is still a valid early-out after reclamation
///
/// The early return fires when the table already holds one span per mapped
/// region, and it is what makes this a length comparison for all but one
/// allocation in several thousand. Reclamation could in principle break it —
/// "one span fewer, one region fewer" leaves the counts equal while the SETS
/// differ — except that [`JitCodeArena::reclaim_region`] removes the reclaimed
/// region's span synchronously, under the arena lock, as part of the same
/// operation that drops the region. So the two sides fall together and cannot
/// become unequal-but-equal-in-count. The unmapping is what makes that
/// mandatory rather than merely tidy: a span that outlived its mapping would
/// route some later, unrelated allocation's free into this arena.
fn publish_arena_spans(arena: &JitCodeArena, handle: &JitArenaHandle) {
    let owner_id = arena.id;
    let mut spans = ARENA_SPANS.lock().unwrap_or_else(|e| e.into_inner());
    if spans.iter().filter(|s| s.owner_id == owner_id).count() == arena.region_count() {
        return;
    }
    spans.retain(|s| s.owner_id != owner_id);
    for r in arena.live_regions() {
        spans.push(ArenaSpan {
            lo: r.base,
            hi: r.end,
            owner_id,
            owner: std::sync::Arc::downgrade(handle),
        });
    }
    ARENA_SPAN_COUNT.store(spans.len(), std::sync::atomic::Ordering::Release);
}

/// If `ptr` is inside some arena's region, hand the block back to that arena
/// and answer `true`.
///
/// `true` means "this address was the arena's business", NOT "a block was
/// recycled". An address inside a region whose arena has been dropped also
/// answers `true`, and leaks: see [`free_executable`] for why reaching
/// `platform_free` with an interior address must never happen.
fn free_if_arena(ptr: *mut u8) -> bool {
    if ARENA_SPAN_COUNT.load(std::sync::atomic::Ordering::Acquire) == 0 {
        return false;
    }
    let addr = ptr as usize;
    // Scoped so the spans guard is released before the arena lock is taken;
    // see `publish_arena_spans`' lock-order note.
    let owner = {
        let spans = ARENA_SPANS.lock().unwrap_or_else(|e| e.into_inner());
        match spans.iter().find(|s| addr >= s.lo && addr < s.hi) {
            Some(s) => s.owner.upgrade(),
            None => return false,
        }
    };
    match owner {
        Some(handle) => {
            handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .release(addr);
        }
        None => {
            tracing::warn!(
                addr = addr,
                "jit code arena: block {addr:#x} was freed after its arena handle was \
                 dropped. The region is still mapped — reclamation runs only from \
                 `JitCodeArena::release`, which a dead arena never reaches, so a \
                 dropped arena's regions are exactly the ones that are never \
                 unmapped — and this is therefore a leak and not a fault. An arena \
                 outliving nothing is the shape that leaks a whole region's worth of \
                 blocks.",
            );
        }
    }
    true
}

// --- the `ExecutableBuffer` constructor ------------------------------------

impl crate::ExecutableBuffer {
    /// Allocate a buffer out of `arena` instead of out of its own OS mapping.
    ///
    /// Identical to [`crate::ExecutableBuffer::new`] in every externally
    /// visible respect except two, both stated here because the repository has
    /// a history of comments that drifted from their code:
    ///
    /// * **Capacity may be larger than asked for.** The buffer's capacity is
    ///   the block's PAGE-ROUNDED size, so a request for 5000 bytes yields a
    ///   buffer of 8192 (at 4 KiB pages). Codegen may use all of it. A method
    ///   that would have set `overflowed` a few hundred bytes past a tight
    ///   estimate therefore compiles here and does not there — a real
    ///   behavioural difference, acceptable only because the arena is
    ///   default-off and because the direction is permissive.
    /// * **`Drop` returns the block to the arena, not to the OS.** That happens
    ///   through [`free_executable`]'s address dispatch rather than through a
    ///   field on the buffer, because this change may not edit
    ///   `jit/src/lib.rs`.
    ///
    /// Everything else is deliberately the same call sequence as `new`:
    /// `COMMITTED_JIT_CODE_BYTES` is bumped by the capacity (so the code-cache
    /// cap sees arena bytes exactly as it sees mapped bytes) and the range is
    /// registered with `jit_code_regions` (so `validate_code_ptr` accepts
    /// pointers into it). Both use the SAME capacity `Drop` will later
    /// deregister and subtract, which is the property that keeps the accounting
    /// balanced.
    ///
    /// With `CRATONVM_JIT_CODE_ARENA` unset this delegates straight to `new`
    /// and the arena is never touched.
    pub fn new_in(arena: &JitArenaHandle, capacity: usize) -> Option<Self> {
        if !jit_code_arena_enabled() {
            return Self::new(capacity);
        }
        let block = {
            let mut guard = arena.lock().unwrap_or_else(|e| e.into_inner());
            let Some(block) = guard.alloc(capacity) else {
                // Counted as OS_REFUSED, not NO_EXTENT_LARGE_ENOUGH, and that
                // is deliberate. [`JitCodeArena::alloc`]'s own contract says
                // its `None` means "size zero, rounding overflow, or the OS
                // refused even a standalone mapping — exactly the cases in
                // which `alloc_executable` would also answer `None`". The
                // arena never refuses for want of a free extent: when the free
                // lists and every region's bump room miss, it maps a new
                // region and then falls back to a standalone mapping. Labelling
                // this NO_EXTENT_LARGE_ENOUGH would put fragmentation's name on
                // an out-of-memory event and send an operator to
                // `external_fragmentation` for a condition that figure cannot
                // explain. See the doc on
                // `crate::code_alloc_failure::NO_EXTENT_LARGE_ENOUGH`.
                //
                // The lock is released first: the ledger is three relaxed
                // `fetch_add`s and has no business inside the arena's critical
                // section, which every compile thread contends for.
                drop(guard);
                crate::note_jit_code_alloc_failure(capacity, crate::code_alloc_failure::OS_REFUSED);
                return None;
            };
            // Must happen under the same lock as the allocation: a concurrent
            // free of a block in a region this allocation just created would
            // otherwise find no span and reach `platform_free` with an interior
            // address.
            publish_arena_spans(&guard, arena);
            block
        };
        let ptr = block.as_ptr();
        // The page-rounded size, not the request: `finalize` will call
        // `make_executable(ptr, capacity)`, and the W^X argument requires that
        // range to be exactly this block's pages.
        let capacity = block.size();
        crate::COMMITTED_JIT_CODE_BYTES.fetch_add(capacity, std::sync::atomic::Ordering::Relaxed);
        // Poison recovered, as at every other use of this lock: skipping the
        // registration after an unrelated panic would make every
        // `validate_code_ptr` into this buffer fail.
        crate::jit_code_regions()
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- protect-failure classification (r9 wave 4, lib4) --------------------

    /// Only a policy denial may latch the process-wide "JIT unavailable"
    /// switch; `ENOMEM`/`EINVAL` (and the uninformative `-1`) decline one
    /// compile.
    ///
    /// The `JitError::AllocationFailed` case this used to assert is gone with
    /// the variant (round 10 wave 8): the assertion was
    /// `assert!(!AllocationFailed.is_policy_denial())` on a value nothing in the
    /// workspace could construct, so it tested a case that could not occur while
    /// reading like coverage of one that could.
    #[test]
    fn only_policy_refusals_classify_as_policy_denials() {
        assert!(!JitError::ProtectFailed(-1).is_policy_denial());
        if cfg!(target_os = "windows") {
            assert!(JitError::ProtectFailed(1655).is_policy_denial());
            assert!(!JitError::ProtectFailed(5).is_policy_denial());
            assert!(!JitError::ProtectFailed(87).is_policy_denial()); // INVALID_PARAMETER
            assert!(!JitError::ProtectFailed(8).is_policy_denial()); // NOT_ENOUGH_MEMORY
        } else {
            assert!(JitError::ProtectFailed(13).is_policy_denial()); // EACCES
            assert!(JitError::ProtectFailed(1).is_policy_denial()); // EPERM
            assert!(!JitError::ProtectFailed(12).is_policy_denial()); // ENOMEM
            assert!(!JitError::ProtectFailed(22).is_policy_denial()); // EINVAL
        }
    }

    // -- the code arena ----------------------------------------------------

    /// An arena with eight-page regions.
    ///
    /// Small on purpose: the tests want "larger than a region" to be a cheap
    /// request and want to see a second region get mapped without allocating
    /// 32 MiB to do it. A test arena that is dropped without emptying leaks the
    /// regions it still holds — there is no `Drop`, for the reason
    /// [`JitCodeArena`] gives — and eight pages apiece is why that is a
    /// rounding error rather than a leak worth caring about.
    ///
    /// These arenas are NOT the process-wide one. Nothing here calls
    /// [`shared_jit_code_arena`], and nothing here publishes spans (that
    /// happens in `ExecutableBuffer::new_in`), so a test arena's regions are
    /// invisible to `free_if_arena` and two tests running concurrently cannot
    /// see each other's blocks.
    fn test_arena() -> JitCodeArena {
        JitCodeArena::with_region_bytes(code_page_size() * 8)
    }

    /// Fill a region's eight pages with eight one-page blocks.
    ///
    /// Used by the reclamation tests, which need "a region that is exactly
    /// full" to be a precise thing rather than an approximation: a region with
    /// bump room left is reclaimable on a different argument (it never handed
    /// those bytes out) and would not exercise the `free_bytes ==
    /// handed_out()` half of [`Region::is_reclaimable`].
    fn fill_one_region(arena: &mut JitCodeArena) -> Vec<ArenaBlock> {
        let page = code_page_size();
        let before = arena.census().regions;
        let blocks: Vec<ArenaBlock> = (0..8).map(|_| arena.alloc(page).expect("fill")).collect();
        assert_eq!(
            arena.census().regions,
            before + 1,
            "eight one-page blocks should fill exactly one new eight-page region"
        );
        blocks
    }

    /// The carve: several blocks out of ONE region, pairwise disjoint.
    ///
    /// Disjointness is checked pairwise and not merely "the addresses differ",
    /// because the failure the arena must not have is two blocks OVERLAPPING —
    /// distinct bases with intersecting extents, which a bump cursor that
    /// advanced by the request rather than the rounded size would produce.
    #[test]
    fn an_arena_serves_several_blocks_from_one_region() {
        if JIT_CODE_ARENA_IS_INERT {
            // Inert by design here: every allocation is its own mapping and
            // there is no region to carve. See `JIT_CODE_ARENA_IS_INERT`.
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();
        let blocks: Vec<ArenaBlock> = (0..4)
            .map(|_| arena.alloc(page / 2).expect("arena alloc"))
            .collect();

        let census = arena.census();
        assert_eq!(
            census.regions, 1,
            "four half-page requests round to four pages and must fit one \
             eight-page region; {} regions were mapped",
            census.regions
        );
        assert_eq!(census.served_from_bump, 4);
        assert_eq!(census.served_standalone, 0);
        for b in &blocks {
            assert_eq!(
                b.origin(),
                BlockOrigin::Region(0),
                "block {:#x} did not come from the one region",
                b.as_ptr() as usize
            );
        }
        for (i, a) in blocks.iter().enumerate() {
            for b in blocks.iter().skip(i + 1) {
                let (a_lo, a_hi) = (a.as_ptr() as usize, a.as_ptr() as usize + a.size());
                let (b_lo, b_hi) = (b.as_ptr() as usize, b.as_ptr() as usize + b.size());
                assert!(
                    a_hi <= b_lo || b_hi <= a_lo,
                    "blocks [{a_lo:#x},{a_hi:#x}) and [{b_lo:#x},{b_hi:#x}) overlap"
                );
            }
        }
        for b in blocks {
            arena.free(b);
        }
    }

    /// Recycling happens, and it happens only within a size class.
    ///
    /// The second half is the one with teeth: a free list that answered a
    /// larger request from a smaller bucket would hand a two-page method a
    /// one-page block, and the overrun would land in whatever the next block
    /// is — silently, because `emit` bounds-checks against `capacity` and
    /// `capacity` would be the lie.
    ///
    /// The direction matters and only one of the two is allowed. A SMALLER
    /// request may now be served from a LARGER free block, by splitting it —
    /// that is `a_split_block_serves_a_smaller_request_and_its_remainder_is_
    /// reusable`. A larger request from a smaller block is the bug, and this
    /// test is what stands between the two.
    #[test]
    fn a_freed_block_is_reused_and_a_larger_request_is_not_served_from_it() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();

        let one = arena.alloc(page).expect("one-page alloc");
        let recycled_addr = one.as_ptr() as usize;
        arena.free(one);
        assert_eq!(arena.census().bytes_free_listed, page);

        let two = arena.alloc(page + 1).expect("two-page alloc");
        assert_eq!(two.size(), page * 2);
        assert_ne!(
            two.as_ptr() as usize,
            recycled_addr,
            "a two-page request was served from the one-page free list"
        );
        assert_eq!(
            arena.census().served_from_free_list,
            0,
            "nothing should have come off a free list yet"
        );
        assert_eq!(
            arena.census().bytes_free_listed,
            page,
            "the one-page block must still be on its own list"
        );

        let again = arena.alloc(page).expect("one-page alloc again");
        assert_eq!(
            again.as_ptr() as usize,
            recycled_addr,
            "the freed one-page block was not reused"
        );
        assert_eq!(arena.census().served_from_free_list, 1);
        assert_eq!(arena.census().bytes_free_listed, 0);

        arena.free(two);
        arena.free(again);
    }

    /// Page alignment and page sizing — asserted, because the whole W^X
    /// argument in this module's arena section rests on them.
    ///
    /// If a block were merely page-ALIGNED and not page-SIZED, the next block
    /// would start mid-page and `make_executable` on the first would flip the
    /// second's first page too.
    #[test]
    fn every_arena_block_is_page_sized_and_page_aligned() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();
        for request in [1usize, 17, page - 1, page, page + 1, page * 3 + 7] {
            let block = arena.alloc(request).expect("arena alloc");
            let addr = block.as_ptr() as usize;
            assert_eq!(
                addr % page,
                0,
                "block {addr:#x} for a {request}-byte request is not page-aligned, \
                 so it can share a page with its neighbour"
            );
            assert_eq!(
                block.size() % page,
                0,
                "block {addr:#x} is {} bytes, not a whole number of {page}-byte pages",
                block.size()
            );
            assert!(block.size() >= request, "a block must cover its request");
            assert!(
                block.size() < request + page,
                "a {request}-byte request took {} bytes; rounding must cost less \
                 than one page",
                block.size()
            );
            assert_eq!(block.requested(), request);
            arena.free(block);
        }
    }

    /// A request no region can hold gets its own mapping instead of `None`.
    ///
    /// Answering `None` would be a compile refused for a reason the caller
    /// cannot act on; the pre-arena allocator would have served it, so the
    /// arena must too. Runs on every platform, including the inert one, where
    /// this is the ONLY path.
    #[test]
    fn an_allocation_larger_than_a_region_falls_back_rather_than_failing() {
        let mut arena = test_arena();
        let oversize = arena.region_bytes() + 1;
        let block = arena
            .alloc(oversize)
            .expect("an over-region request must fall back to a standalone mapping");
        assert_eq!(block.origin(), BlockOrigin::Standalone);
        assert!(block.size() >= oversize);
        let census = arena.census();
        assert_eq!(census.served_standalone, 1);
        assert_eq!(
            census.regions, 0,
            "a request that cannot fit a region must not have mapped one"
        );
        arena.free(block);
    }

    /// Every reserved byte is accounted for, in exactly one place.
    ///
    /// A census whose parts do not sum is worse than no census: it will be used
    /// to argue that the arena does or does not pay for itself, and it will be
    /// wrong by whatever it silently drops.
    #[test]
    fn the_census_partitions_every_reserved_byte() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();
        let a = arena.alloc(page / 2 + 1).expect("alloc a");
        let b = arena.alloc(page * 2 - 3).expect("alloc b");
        let c = arena.alloc(7).expect("alloc c");
        arena.free(b);

        let census = arena.census();
        assert_eq!(
            census.bytes_requested_live
                + census.bytes_padding_live
                + census.bytes_free_listed
                + census.bytes_never_bumped
                + census.bytes_stranded,
            census.bytes_usable,
            "the census does not partition the usable bytes: {census:?}"
        );
        assert_eq!(
            census.bytes_stranded, 0,
            "nothing here should have failed to go back to writable: {census:?}"
        );
        assert_eq!(
            census.bytes_usable + census.bytes_alignment_waste,
            census.bytes_mapped,
            "mapped bytes are not usable bytes plus alignment waste: {census:?}"
        );
        assert_eq!(
            census.bytes_live,
            census.bytes_requested_live + census.bytes_padding_live
        );
        assert_eq!(census.live_blocks, 2);
        assert_eq!(census.bytes_live, page * 2, "a and c are one page each");
        assert_eq!(census.bytes_free_listed, page * 2, "b was two pages");
        assert_eq!(census.bytes_never_bumped, page * 4, "eight pages less four");
        assert_eq!(
            census.bytes_requested_live,
            (page / 2 + 1) + 7,
            "the requested total must be what the callers asked for, not what \
             they were given"
        );

        arena.free(a);
        arena.free(c);
        let census = arena.census();
        assert_eq!(census.bytes_live, 0);
        assert_eq!(
            census.bytes_requested_live
                + census.bytes_padding_live
                + census.bytes_free_listed
                + census.bytes_never_bumped
                + census.bytes_stranded,
            census.bytes_usable,
            "the identity must survive every block being returned: {census:?}"
        );
        // The region is now empty, and the hysteresis rule kept it: one empty
        // region is the spare. So `bytes_usable` is still a region's worth and
        // the identity above is over something rather than over nothing, which
        // is the only reason this assertion is here.
        assert_eq!(census.regions, 1, "the spare must survive: {census:?}");
        assert_eq!(census.empty_regions, 1);
        assert_eq!(census.regions_reclaimed, 0);
        assert_eq!(census.bytes_usable, page * 8);
    }

    /// A recycled block is WRITABLE, even though the method that had it before
    /// left it executable.
    ///
    /// This is the one arena test that would have failed with an access
    /// violation rather than an assertion before `release` learned to restore
    /// the protection. `ExecutableBuffer::drop` never calls `make_writable` —
    /// it had no reason to, because before the arena a retired mapping went
    /// back to the OS and the next buffer got fresh RW pages — so a block that
    /// had been through `finalize` came back to the free list still RX, and the
    /// next compile's first `emit` would have faulted. The `finalize` here is
    /// what makes the test mean anything: without it the block would be RW by
    /// accident and the write would prove nothing.
    #[test]
    fn a_recycled_block_is_writable_again_after_its_predecessor_was_executable() {
        if JIT_CODE_ARENA_IS_INERT {
            // No per-page protection to restore: W^X there is the thread-wide
            // `pthread_jit_write_protect_np` toggle, and the arena is inert
            // anyway. See `JIT_CODE_ARENA_IS_INERT`.
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();

        let first = arena.alloc(page).expect("first block");
        let addr = first.as_ptr() as usize;
        {
            let _write = JitWriteScope::enter();
            // SAFETY: a live block this arena handed out, still RW from its
            // region's initial mapping, and one byte is inside it.
            unsafe { *first.as_ptr() = 0x90 };
        }
        // Exactly what a real compile does at the end of codegen.
        make_executable(first.as_ptr(), first.size()).expect("make_executable");
        arena.free(first);

        let second = arena.alloc(page).expect("second block");
        assert_eq!(
            second.as_ptr() as usize,
            addr,
            "the one-page block must have been recycled, or this test is \
             writing into fresh memory and proving nothing"
        );
        {
            let _write = JitWriteScope::enter();
            // SAFETY: a live block this arena handed out. It is writable
            // because `release` restored the protection the `make_executable`
            // above took away — which is the whole assertion. If that ever
            // regresses this line faults instead of failing, which is why the
            // address check above comes first: it proves the block really is
            // the recycled one.
            unsafe { *second.as_ptr() = 0xCC };
        }
        assert_eq!(arena.census().served_from_free_list, 1);
        assert_eq!(
            arena.census().stranded_blocks,
            0,
            "the protection flip must have succeeded, not stranded the block"
        );
        arena.free(second);
    }

    // -- region reclamation ------------------------------------------------

    /// A region whose every block has been freed is unmapped — but only once a
    /// SECOND region has gone empty, because one empty region is kept as a
    /// spare.
    ///
    /// Both halves matter and they pull in opposite directions, which is why
    /// they are asserted in one test rather than two. Without reclamation a run
    /// that compiles a burst and quiesces holds every region forever; with
    /// EAGER reclamation an arena that oscillates around one live method
    /// unmaps and re-maps a region per method, which is worse than either. The
    /// rule is [`JIT_CODE_ARENA_EMPTY_REGION_SPARES`] and this test pins both
    /// its trigger (more than one empty) and its floor (reclaim down to one).
    #[test]
    fn an_empty_region_is_reclaimed_but_one_spare_is_kept() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();

        let first = fill_one_region(&mut arena);
        let second = fill_one_region(&mut arena);
        assert_eq!(arena.census().regions, 2);
        assert_eq!(arena.census().live_blocks, 16);
        assert_eq!(
            arena.census().empty_regions,
            0,
            "both regions are entirely live"
        );

        // Empty the FIRST region completely. One empty region is the spare, so
        // nothing may be unmapped yet — this is the half that a naive "free the
        // region as soon as it empties" implementation gets wrong.
        for b in first {
            arena.free(b);
        }
        let census = arena.census();
        assert_eq!(
            census.regions, 2,
            "the first region to empty is the spare and must NOT be unmapped: \
             {census:?}"
        );
        assert_eq!(census.empty_regions, 1);
        assert_eq!(census.regions_reclaimed, 0);
        assert_eq!(census.bytes_free_listed, page * 8);

        // Empty the second. Now two are empty, which is one more than the rule
        // allows, so exactly one is reclaimed and exactly one is left.
        for b in second {
            arena.free(b);
        }
        let census = arena.census();
        assert_eq!(
            census.regions, 1,
            "with two empty regions one must be reclaimed: {census:?}"
        );
        assert_eq!(census.regions_reclaimed, 1);
        assert_eq!(
            census.bytes_reclaimed as usize,
            page * 8,
            "the reclaimed region's whole mapping is accounted for: {census:?}"
        );
        assert_eq!(census.empty_regions, 1, "the spare stays empty and mapped");
        assert_eq!(census.live_blocks, 0);
        assert_eq!(
            census.bytes_free_listed,
            page * 8,
            "only the surviving region's blocks may still be on a free list: \
             {census:?}"
        );
        assert_eq!(
            census.bytes_requested_live
                + census.bytes_padding_live
                + census.bytes_free_listed
                + census.bytes_never_bumped
                + census.bytes_stranded,
            census.bytes_usable,
            "the partition must hold across a reclamation: {census:?}"
        );
    }

    /// No free-list entry may survive its region.
    ///
    /// This is the one that would be a wild write rather than a failed
    /// assertion if it regressed: an entry left behind by
    /// [`JitCodeArena::reclaim_region`] names an address inside memory that
    /// `platform_free` has just returned to the OS, and the next allocation of
    /// that size class would pop it and hand it to a compiler as a place to
    /// write instructions. So the check is made against the reclaimed region's
    /// actual address range and not merely against a count.
    #[test]
    fn a_reclaimed_regions_blocks_are_gone_from_the_free_list() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();

        let first = fill_one_region(&mut arena);
        let second = fill_one_region(&mut arena);
        let second_lo = second
            .iter()
            .map(|b| b.as_ptr() as usize)
            .min()
            .expect("eight blocks");
        let second_hi = second
            .iter()
            .map(|b| b.as_ptr() as usize + b.size())
            .max()
            .expect("eight blocks");
        let first_lo = first
            .iter()
            .map(|b| b.as_ptr() as usize)
            .min()
            .expect("eight blocks");

        for b in first {
            arena.free(b);
        }
        for b in second {
            arena.free(b);
        }
        assert_eq!(arena.census().regions_reclaimed, 1);

        // The surviving region is the lower-indexed one — both have zero bump
        // room, so the spare rule's tie-break picks the lowest slot — so the
        // reclaimed range is the second region's.
        let surviving: Vec<(usize, usize)> = arena
            .free_by_pages
            .iter()
            .flat_map(|list| list.iter().copied())
            .collect();
        assert_eq!(
            surviving.len(),
            8,
            "only the spare's eight one-page blocks may remain: {surviving:?}"
        );
        for &(addr, region) in &surviving {
            assert!(
                addr < second_lo || addr >= second_hi,
                "free entry {addr:#x} lies inside the reclaimed region \
                 [{second_lo:#x},{second_hi:#x}) — it points at unmapped memory"
            );
            assert!(
                addr >= first_lo && addr < first_lo + page * 8,
                "free entry {addr:#x} is not inside the surviving region either"
            );
            assert!(
                arena.regions[region].is_some(),
                "free entry {addr:#x} names reclaimed region slot {region}"
            );
        }

        // And the slot the reclaimed region vacated is REUSED rather than
        // appended past: the next region that has to be mapped takes it, so the
        // slot vector does not grow without bound over a long run. The request
        // is a whole region's worth on purpose — anything smaller would be
        // served from the spare's free list and map nothing.
        let slots_before = arena.regions.len();
        let whole = arena.alloc(page * 8).expect("a whole region's worth");
        assert_eq!(
            arena.census().regions,
            2,
            "an eight-page request fits no free block and no bump cursor, so a \
             region had to be mapped"
        );
        assert_eq!(
            arena.regions.len(),
            slots_before,
            "a vacated region slot must be reused, not appended past"
        );
        arena.free(whole);
    }

    /// A region that is still handing blocks out is never reclaimed, even when
    /// other regions are empty.
    ///
    /// The rule the arena must keep is not "reclaim empty regions" but "never
    /// unmap a region anything is still using". A single live block anywhere in
    /// a region pins the whole 16 MiB (8 pages here), and that is correct: the
    /// unit of unmapping is the mapping.
    #[test]
    fn a_region_with_one_live_block_is_never_reclaimed() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();

        let first = fill_one_region(&mut arena);
        let mut second = fill_one_region(&mut arena);
        let third = fill_one_region(&mut arena);
        assert_eq!(arena.census().regions, 3);

        // Keep exactly one block of the middle region alive.
        let pinned = second.pop().expect("eight blocks");

        for b in first {
            arena.free(b);
        }
        for b in second {
            arena.free(b);
        }
        for b in third {
            arena.free(b);
        }

        let census = arena.census();
        assert_eq!(census.live_blocks, 1, "only the pinned block is live");
        assert_eq!(
            census.regions, 2,
            "two of three regions were empty, so exactly one is reclaimed and \
             one is kept as the spare; the pinned region is neither: {census:?}"
        );
        assert_eq!(census.regions_reclaimed, 1);
        assert_eq!(
            census.empty_regions, 1,
            "the pinned region does not count as empty: {census:?}"
        );
        // The pinned block is still usable: its region is still mapped, so a
        // write into it is defined. This is the assertion that would fault
        // rather than fail if a region with a live block were ever unmapped.
        {
            let _write = JitWriteScope::enter();
            // SAFETY: `pinned` is a live block this arena handed out and never
            // took back; one byte is inside it, and it has not been made
            // executable, so it is still writable.
            unsafe { *pinned.as_ptr() = 0x90 };
        }
        assert_eq!(
            census.bytes_free_listed,
            page * 8 + page * 7,
            "the spare's eight pages plus the pinned region's seven freed ones: \
             {census:?}"
        );
        arena.free(pinned);
    }

    // -- splitting ---------------------------------------------------------

    /// A larger free block serves a smaller request, and its remainder is
    /// reusable.
    ///
    /// This is what exact-fit-only could not do: before splitting, a 4-page
    /// block on the free list was dead weight to every request that was not for
    /// exactly 4 pages, so a workload whose method sizes drifted stranded a
    /// class at a time. Both pieces are checked for page alignment and page
    /// sizing, because the W^X argument at the top of this file is an induction
    /// and a split is the step that could break it.
    #[test]
    fn a_split_block_serves_a_smaller_request_and_its_remainder_is_reusable() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();

        let big = arena.alloc(page * 4).expect("four-page alloc");
        let big_addr = big.as_ptr() as usize;
        assert_eq!(big.size(), page * 4);
        arena.free(big);
        assert_eq!(arena.census().bytes_free_listed, page * 4);
        assert_eq!(arena.census().free_blocks, 1);

        // One page out of the four-page block: the front piece, at the same
        // address, with three pages pushed back as a new class-3 entry.
        let small = arena.alloc(page).expect("one-page alloc");
        assert_eq!(
            small.as_ptr() as usize,
            big_addr,
            "a one-page request must be served from the head of the four-page \
             free block, not from the bump cursor"
        );
        assert_eq!(small.size(), page);
        let census = arena.census();
        assert_eq!(census.blocks_split, 1);
        assert_eq!(census.served_from_free_list, 1);
        assert_eq!(
            census.served_from_bump, 1,
            "only the original four-page block came off the cursor: {census:?}"
        );
        assert_eq!(
            census.bytes_free_listed,
            page * 3,
            "the remainder must be back on a free list: {census:?}"
        );
        assert_eq!(
            census.free_blocks, 1,
            "one remainder, not three: {census:?}"
        );

        // The remainder is reusable, at exactly the address the split left it.
        let rest = arena.alloc(page * 3).expect("three-page alloc");
        assert_eq!(
            rest.as_ptr() as usize,
            big_addr + page,
            "the remainder must be handed out at the split point"
        );
        assert_eq!(rest.size(), page * 3);
        assert_eq!(arena.census().served_from_free_list, 2);
        assert_eq!(arena.census().blocks_split, 1, "the second fit exactly");
        assert_eq!(arena.census().bytes_free_listed, 0);

        // Both pieces: page-aligned, page-sized, disjoint, and together exactly
        // the block they were cut from.
        for piece in [&small, &rest] {
            let addr = piece.as_ptr() as usize;
            assert_eq!(addr % page, 0, "split piece {addr:#x} is not page-aligned");
            assert_eq!(
                piece.size() % page,
                0,
                "split piece {addr:#x} is {} bytes, not whole pages",
                piece.size()
            );
        }
        let small_hi = small.as_ptr() as usize + small.size();
        assert_eq!(small_hi, rest.as_ptr() as usize, "the pieces must abut");
        assert_eq!(
            rest.as_ptr() as usize + rest.size(),
            big_addr + page * 4,
            "the pieces must together cover exactly the original block"
        );

        arena.free(small);
        arena.free(rest);
    }

    /// Splitting narrows the free lists and nothing widens them again.
    ///
    /// The honest half of the splitting change: there is no coalescing, so the
    /// two one-page pieces this leaves behind are never merged back into a
    /// two-page block, and a subsequent two-page request has to take fresh bump
    /// room even though two adjacent free pages exist. The test exists so that
    /// the limit is a pinned, visible fact rather than a sentence in a doc
    /// comment that nobody can check.
    #[test]
    fn freed_neighbours_are_not_coalesced() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();

        let two = arena.alloc(page * 2).expect("two-page alloc");
        let two_addr = two.as_ptr() as usize;
        arena.free(two);
        // Split it into two adjacent one-page blocks, then free both.
        let lo = arena.alloc(page).expect("lo");
        let hi = arena.alloc(page).expect("hi");
        assert_eq!(lo.as_ptr() as usize, two_addr);
        assert_eq!(hi.as_ptr() as usize, two_addr + page);
        arena.free(lo);
        arena.free(hi);

        let census = arena.census();
        assert_eq!(census.free_blocks, 2, "two adjacent one-page blocks");
        assert_eq!(census.bytes_free_listed, page * 2);

        let again = arena.alloc(page * 2).expect("two-page alloc again");
        assert_ne!(
            again.as_ptr() as usize,
            two_addr,
            "a two-page request was served from two adjacent one-page free \
             blocks — this arena does not coalesce, so if that ever becomes \
             true the merge has to be reviewed for the W^X argument"
        );
        let census = arena.census();
        assert_eq!(
            census.bytes_free_listed,
            page * 2,
            "the two one-page blocks are still stranded: {census:?}"
        );
        assert_eq!(
            census.served_from_bump, 2,
            "the second two-page request had to take fresh bump room: {census:?}"
        );
        arena.free(again);
    }

    // -- the pressure report and the eviction seam -------------------------

    /// The pressure report and the census must not be able to disagree.
    ///
    /// They are computed by different code over the same state — the census is
    /// a full O(live blocks) partition meant for a report point, the pressure
    /// report is a cheap subset read on the allocation path — and the whole
    /// value of the cheap one is that a policy can trust it. A mixed sequence
    /// of allocations, frees, a split and a fallback is run first so that the
    /// comparison is over a state with something in every bucket.
    #[test]
    fn the_pressure_report_agrees_with_the_census() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();

        let a = arena.alloc(page * 3).expect("a");
        let b = arena.alloc(page).expect("b");
        arena.free(a); // three pages to the free list
        let c = arena.alloc(page).expect("c"); // splits a's block
        let d = arena.alloc(page * 5).expect("d"); // forces a second region
        assert_eq!(arena.census().blocks_split, 1);
        assert_eq!(arena.census().regions, 2);

        let census = arena.census();
        let pressure = arena.pressure();
        assert_eq!(pressure.arena_id, census.arena_id);
        assert_eq!(pressure.bytes_reserved, census.bytes_mapped);
        assert_eq!(pressure.bytes_live, census.bytes_live);
        assert_eq!(pressure.bytes_free_listed, census.bytes_free_listed);
        assert_eq!(pressure.regions, census.regions);
        assert_eq!(pressure.region_bytes, census.region_bytes);
        assert_eq!(pressure.live_blocks, census.live_blocks);
        assert!(
            pressure.last_alloc_mapped_region,
            "`d` did not fit either existing region and mapped one: {pressure:?}"
        );
        assert!(!pressure.last_alloc_was_standalone);

        // The cumulative counters are over every allocation, not over the live
        // ones, so they are checked against the requests this test actually
        // made rather than against the partition.
        assert_eq!(
            census.bytes_requested_total as usize,
            page * 3 + page + page + page * 5,
            "five requests, all of them exact page multiples: {census:?}"
        );
        assert_eq!(
            census.bytes_served_total, census.bytes_requested_total,
            "page-multiple requests cost no rounding: {census:?}"
        );
        assert_eq!(census.bytes_fragmentation_total, 0);

        // Now a request that is NOT a page multiple, so the fragmentation
        // counter has something to count.
        let e = arena.alloc(page + 1).expect("e");
        let census = arena.census();
        assert_eq!(
            census.bytes_fragmentation_total as usize,
            page - 1,
            "a (page + 1)-byte request takes two pages: {census:?}"
        );
        assert_eq!(
            census.bytes_fragmentation_total,
            census.bytes_served_total - census.bytes_requested_total
        );

        // And a standalone fallback, which the pressure report must report as
        // such: the arena declined to serve it, which is the condition an
        // eviction policy most wants to know about.
        let big = arena.alloc(arena.region_bytes() + 1).expect("standalone");
        let pressure = arena.pressure();
        assert!(pressure.last_alloc_was_standalone);
        assert!(!pressure.last_alloc_mapped_region);
        assert_eq!(
            pressure.bytes_reserved,
            arena.census().bytes_mapped,
            "a standalone mapping is not a region and must not be counted as \
             reserved arena space"
        );

        for block in [b, c, d, e, big] {
            arena.free(block);
        }
    }

    /// With no hook installed the arena never asks anyone anything.
    ///
    /// The point of the assertion is the DEFAULT, not the counter: a seam whose
    /// presence changed behaviour would not be a seam. An arena that maps three
    /// regions and falls back once must record zero eviction requests.
    #[test]
    fn the_eviction_hook_is_not_called_when_unset() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();
        assert!(!arena.has_eviction_hook());

        let blocks: Vec<ArenaBlock> = (0..17).map(|_| arena.alloc(page).expect("alloc")).collect();
        let over = arena.alloc(arena.region_bytes() + 1).expect("standalone");
        let census = arena.census();
        assert!(census.regions >= 3, "the run must have grown: {census:?}");
        assert_eq!(census.served_standalone, 1);
        assert_eq!(
            census.evictions_requested, 0,
            "an arena with no hook must never ask: {census:?}"
        );
        assert_eq!(census.evictions_performed, 0);

        for b in blocks {
            arena.free(b);
        }
        arena.free(over);
    }

    /// The hook is called exactly once per region the arena is about to create,
    /// and not at all for an allocation the arena can already serve.
    ///
    /// "Before a new region is created" is the contract, and the test checks it
    /// from both sides: the count goes up in lockstep with `regions`, and an
    /// allocation served from the bump cursor or from a free list leaves it
    /// alone. The hook itself only touches an atomic — a hook that reached back
    /// into this arena would deadlock on the mutex `alloc` holds, which is the
    /// limitation [`JitCodeArenaEvictionHook`] documents.
    #[test]
    fn the_eviction_hook_is_called_once_before_each_new_region() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_regions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
        {
            let calls = std::sync::Arc::clone(&calls);
            let seen_regions = std::sync::Arc::clone(&seen_regions);
            let previous =
                arena.set_eviction_hook(Box::new(move |pressure: &JitCodeArenaPressure| {
                    calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    seen_regions.store(pressure.regions, std::sync::atomic::Ordering::Relaxed);
                    // "I evicted nothing", which is the only honest answer a
                    // hook that cannot free into a locked arena can give.
                    false
                }));
            assert!(previous.is_none());
        }
        assert!(arena.has_eviction_hook());

        // First allocation: nothing to serve it from, so the arena is about to
        // map its first region and asks first.
        let first = arena.alloc(page).expect("first");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            seen_regions.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the hook is called BEFORE the region exists, so it must see zero"
        );
        assert_eq!(arena.census().regions, 1);

        // The next seven fit the region that was just mapped, so nobody is
        // asked anything.
        let rest: Vec<ArenaBlock> = (0..7).map(|_| arena.alloc(page).expect("rest")).collect();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "an allocation the arena can already serve must not ask"
        );
        assert_eq!(arena.census().regions, 1);

        // The ninth does not fit, so the arena asks a second time and then maps
        // a second region.
        let ninth = arena.alloc(page).expect("ninth");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(
            seen_regions.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the second call must see the one region that already exists"
        );
        assert_eq!(arena.census().regions, 2);

        let census = arena.census();
        assert_eq!(census.evictions_requested, 2);
        assert_eq!(
            census.evictions_performed, 0,
            "the hook answered `false` every time: {census:?}"
        );

        // A recycled allocation does not ask either: the free list is checked
        // before the seam fires.
        arena.free(first);
        let recycled = arena.alloc(page).expect("recycled");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "an allocation served from a free list must not ask"
        );

        // Removing the hook restores the default, and the counters keep what
        // they measured rather than resetting.
        assert!(arena.take_eviction_hook().is_some());
        assert!(!arena.has_eviction_hook());
        let after = arena.alloc(page * 8).expect("after");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "a removed hook must not be called"
        );
        assert_eq!(arena.census().evictions_requested, 2);

        for b in rest {
            arena.free(b);
        }
        arena.free(ninth);
        arena.free(recycled);
        arena.free(after);
    }

    /// A hook that claims to have evicted something is counted as having done
    /// so, and the arena still gets its region.
    ///
    /// This pins the honest reading of `evictions_performed`: it counts what
    /// the OWNER said, not what the arena observed. A hook that returns `true`
    /// without actually putting anything back on a free list — which is all a
    /// hook can do while the arena lock is held — still ends with a region
    /// mapped, and the census says so. Anyone using these two numbers to argue
    /// that the seam is paying for itself has to compare
    /// `evictions_performed` against `regions`, not read it on its own.
    #[test]
    fn an_eviction_that_frees_nothing_still_maps_a_region() {
        if JIT_CODE_ARENA_IS_INERT {
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();
        assert!(arena
            .set_eviction_hook(Box::new(|_: &JitCodeArenaPressure| true))
            .is_none());

        let block = arena.alloc(page).expect("alloc");
        let census = arena.census();
        assert_eq!(census.evictions_requested, 1);
        assert_eq!(
            census.evictions_performed, 1,
            "the hook said it evicted, so the arena records that it did: \
             {census:?}"
        );
        assert_eq!(
            census.regions, 1,
            "nothing was actually freed, so the region was mapped anyway: \
             {census:?}"
        );
        arena.free(block);
    }

    /// Two neighbouring arena blocks are on DISTINCT pages, so a protection
    /// flip on one cannot reach the other.
    ///
    /// The address comparison is the checkable half, and it is the half the
    /// W^X argument is actually made of: `VirtualProtect` / `mprotect` round to
    /// page boundaries, so "different pages" is exactly "cannot be reached".
    /// The flip that follows exercises the real calls and then writes one byte
    /// into the neighbour; that write is safe ONLY because the assertion above
    /// it already established the two are not on one page, so a regression
    /// shows up here as an access violation rather than as a failed assertion.
    /// That asymmetry is why the assertion comes first and is not the
    /// afterthought.
    #[test]
    fn make_executable_on_one_arena_block_cannot_reach_its_neighbour() {
        if JIT_CODE_ARENA_IS_INERT {
            // There is no per-page protection on macOS/ARM64 at all — W^X is
            // the thread-wide `pthread_jit_write_protect_np` toggle — and the
            // arena is inert there anyway.
            return;
        }
        let mut arena = test_arena();
        let page = code_page_size();
        let first = arena.alloc(8).expect("first block");
        let second = arena.alloc(8).expect("second block");

        let first_last_page = (first.as_ptr() as usize + first.size() - 1) / page;
        let second_first_page = second.as_ptr() as usize / page;
        assert!(
            first_last_page < second_first_page,
            "block {:#x}+{} ends on page {first_last_page} and block {:#x} starts \
             on page {second_first_page}; they share a page, so make_executable \
             on the first would flip the second",
            first.as_ptr() as usize,
            first.size(),
            second.as_ptr() as usize,
        );

        make_executable(first.as_ptr(), first.size()).expect("make_executable");
        {
            let _write = JitWriteScope::enter();
            // SAFETY: `second` is a live block this arena handed out, one byte
            // is inside it, and the assertion above proves it is not on a page
            // the `make_executable` just flipped.
            unsafe { *second.as_ptr() = 0x90 };
        }
        make_writable(first.as_ptr(), first.size()).expect("make_writable");

        arena.free(first);
        arena.free(second);
    }

    /// Cells must be distinct, cache-line aligned, and zero.
    ///
    /// Zero is the part with teeth: `CacheLineFlag::new` writes the initial
    /// value itself, but every other prospective caller (a counter, an epoch)
    /// will read the cell before writing it, and "fresh anonymous pages are
    /// zero-filled" is a property of the FIRST cell of a chunk, not of the
    /// 900th.
    #[test]
    fn code_adjacent_cells_are_distinct_aligned_and_zero() {
        if near_globals_engaged() {
            // The allocator declines by design here; there are no cells to
            // assert about. See `alloc_code_adjacent_cell`.
            return;
        }
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            let cell = alloc_code_adjacent_cell().expect("cell allocation failed");
            let addr = cell as usize;
            assert_eq!(
                addr % CODE_ADJACENT_CELL_SIZE,
                0,
                "cell {addr:#x} is not cache-line aligned, so it can share a \
                 line with its neighbour"
            );
            assert!(seen.insert(addr), "cell {addr:#x} was handed out twice");
            // SAFETY: a freshly carved cell of exactly this size that nothing
            // else holds a reference to.
            let bytes = unsafe { std::slice::from_raw_parts(cell, CODE_ADJACENT_CELL_SIZE) };
            assert!(
                bytes.iter().all(|&b| b == 0),
                "cell {addr:#x} is not zeroed"
            );
            // Dirty it, so the next iteration's zero check is about the
            // allocator rather than about untouched pages.
            // SAFETY: as above; this cell is ours for the rest of the process.
            unsafe { std::ptr::write_bytes(cell, 0xA5, CODE_ADJACENT_CELL_SIZE) };
        }
    }

    /// The whole point: a cell must land within `disp32` reach of JIT code.
    ///
    /// This is the property `jit/src/x64/safepoint.rs::emit_safepoint_poll`
    /// silently loses when it does not hold — the poll stays correct and grows
    /// from 7 bytes to 15 plus a clobbered register, at every loop back edge
    /// and method entry in the process. Nothing else in the tree fails, which
    /// is exactly why it went unnoticed from 2026-09-02 to 2026-09-10 with the
    /// flag on the mimalloc heap, ~124 TB from the code cache.
    ///
    /// x86-64 only: `disp32` reach is what makes the distance matter, and
    /// aarch64's `ADRP` has its own (±4 GB) window that no emitter uses yet.
    ///
    /// Both sides come from `platform_alloc`, so this asserts that the OS puts
    /// two allocations from one primitive in one region. That is true of every
    /// mapper we run on but is not architecturally guaranteed; if it ever
    /// fails, the fallback still runs and this test is the notification.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_cell_is_within_disp32_of_a_code_buffer() {
        if near_globals_engaged() {
            // `near_globals` owns placement and this allocator declines, so
            // there is no cell whose distance means anything. Reach in that
            // configuration is that strategy's own claim, asserted by
            // `near_globals_tests`.
            return;
        }
        let code = alloc_executable(4096).expect("alloc_executable failed");
        let cell = alloc_code_adjacent_cell().expect("cell allocation failed");
        // Widening: usize -> i128, so the subtraction cannot wrap.
        let delta = (cell as usize as i128) - (code as usize as i128);
        let in_reach = delta >= i32::MIN as i128 && delta <= i32::MAX as i128;
        free_executable(code, 4096);
        assert!(
            in_reach,
            "code buffer {:#x} and data cell {:#x} are {:.1} GB apart; a \
             RIP-relative poll cannot reach, so every safepoint poll in the \
             process falls back to MOV r64, imm64",
            code as usize,
            cell as usize,
            (delta.unsigned_abs() as f64) / (1024.0 * 1024.0 * 1024.0),
        );
    }

    #[test]
    fn alloc_write_free() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");
        assert!(!ptr.is_null());

        // Write a pattern (memory starts as RW; on macOS/ARM64 the scope is
        // what makes it writable for this thread).
        {
            let _write = JitWriteScope::enter();
            // SAFETY: `ptr` is a fresh `size`-byte allocation that nothing
            // else references, and every index is below `size`.
            unsafe {
                for i in 0..size {
                    *ptr.add(i) = (i & 0xFF) as u8;
                }
            }
        }

        // Finalize: transition from RW to RX
        make_executable(ptr, size).expect("make_executable failed");

        // Read back (reading is allowed in both RW and RX)
        // SAFETY: as above; reads stay in bounds and the region is readable.
        unsafe {
            for i in 0..size {
                assert_eq!(*ptr.add(i), (i & 0xFF) as u8);
            }
        }

        free_executable(ptr, size);
    }

    /// `MAP_ANONYMOUS` differs per OS, and the table must say so on EVERY host.
    ///
    /// The Unix allocator used Linux's `0x20` for Linux, FreeBSD and macOS
    /// x86-64 alike. On the BSDs and Darwin that is not `MAP_ANON`, `mmap`
    /// failed with `EINVAL`, and the JIT was silently dead on those systems.
    /// This checks the table itself, so a Windows or Linux CI run catches a
    /// regression for a platform it cannot execute.
    #[test]
    fn map_anonymous_follows_each_os_abi() {
        assert_eq!(
            map_anonymous(UnixMmapAbi::Linux),
            0x20,
            "linux <asm-generic/mman-common.h>"
        );
        assert_eq!(
            map_anonymous(UnixMmapAbi::LinuxMips),
            0x800,
            "linux <asm/mman.h> on MIPS"
        );
        assert_eq!(
            map_anonymous(UnixMmapAbi::Bsd),
            0x1000,
            "Darwin and FreeBSD <sys/mman.h>"
        );
        assert_eq!(
            map_anonymous(UnixMmapAbi::Solaris),
            0x100,
            "illumos <sys/mman.h>"
        );
        assert_ne!(
            map_anonymous(UnixMmapAbi::Linux),
            map_anonymous(UnixMmapAbi::Bsd),
            "the two families genuinely differ -- one constant cannot serve both"
        );
    }

    /// The host picks the family its own headers use.
    #[test]
    fn the_host_mmap_abi_is_this_targets_own() {
        #[cfg(all(target_os = "linux", not(target_arch = "mips64")))]
        assert_eq!(HOST_MMAP_ABI, Some(UnixMmapAbi::Linux));
        #[cfg(any(target_os = "macos", target_os = "freebsd"))]
        assert_eq!(HOST_MMAP_ABI, Some(UnixMmapAbi::Bsd));
        #[cfg(target_os = "windows")]
        assert_eq!(HOST_MMAP_ABI, None, "Windows never maps through mmap");
    }

    /// A failed `mprotect` reports `errno`, not its `-1` return value.
    ///
    /// A pointer one byte into a page is not page-aligned, which POSIX makes
    /// `EINVAL` (22 on Linux, the BSDs and Darwin alike).
    #[cfg(all(
        not(target_os = "windows"),
        not(all(target_os = "macos", target_arch = "aarch64"))
    ))]
    #[test]
    fn a_protect_failure_carries_errno() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");
        // SAFETY: one byte into a 4096-byte allocation is in bounds.
        let misaligned = unsafe { ptr.add(1) };
        match make_executable(misaligned, size - 1) {
            Err(JitError::ProtectFailed(code)) => {
                assert_ne!(
                    code, -1,
                    "the payload must be errno, not mprotect's return value"
                );
                assert_eq!(code, 22, "a misaligned mprotect is EINVAL");
            }
            other => panic!("a misaligned mprotect must fail, got {other:?}"),
        }
        free_executable(ptr, size);
    }

    /// Scopes nest, and a write inside the innermost one lands.
    #[test]
    fn jit_write_scopes_nest() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");
        {
            let _outer = JitWriteScope::enter();
            {
                let _inner = JitWriteScope::enter();
                // SAFETY: in bounds of a fresh allocation nothing else holds.
                unsafe { *ptr = 0x5A };
            }
            // Still inside the outer scope: the inner drop must not have
            // re-protected the page for this thread.
            // SAFETY: as above.
            unsafe { *ptr.add(1) = 0xA5 };
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            assert_eq!(JIT_WRITE_DEPTH.with(|d| d.get()), 1);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(
            JIT_WRITE_DEPTH.with(|d| d.get()),
            0,
            "the thread is back in execute mode"
        );
        make_executable(ptr, size).expect("make_executable failed");
        // SAFETY: reads in bounds of a readable region.
        unsafe {
            assert_eq!(*ptr, 0x5A);
            assert_eq!(*ptr.add(1), 0xA5);
        }
        free_executable(ptr, size);
    }

    /// A zero-length request must be REFUSED, on every platform we build for.
    ///
    /// The old name (`alloc_zero_returns_some`) was the lie: no backend can
    /// satisfy it. `VirtualAlloc(NULL, 0, ...)` fails with
    /// `ERROR_INVALID_PARAMETER` (87), and POSIX specifies `mmap` with
    /// `len == 0` fails with `EINVAL` — so both `platform_alloc` bodies take
    /// their null / `MAP_FAILED` arm and answer `None`. The failure is that a
    /// caller ever gets a `Some` it would then write code into and hand to
    /// `make_executable` on a zero-byte region.
    ///
    /// Was vacuous: `if let Some(ptr) = alloc_executable(0) { free… }` — the
    /// interesting branch was the silent one, so deleting the `p.is_null()` /
    /// `p == MAP_FAILED` check in `platform_alloc` (returning `Some(null)`)
    /// stayed green.
    #[test]
    fn alloc_zero_is_refused() {
        assert!(
            alloc_executable(0).is_none(),
            "a zero-length JIT allocation must be refused, not handed back"
        );
    }

    #[test]
    fn make_executable_then_writable_roundtrip() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");

        // Write initial data
        {
            let _write = JitWriteScope::enter();
            // SAFETY: in bounds of a fresh allocation nothing else references.
            unsafe {
                *ptr = 0xAA;
            }
        }

        // Transition RW -> RX
        make_executable(ptr, size).expect("make_executable failed");

        // Read should still work
        // SAFETY: an in-bounds read of a readable region.
        unsafe {
            assert_eq!(*ptr, 0xAA);
        }

        // Transition RX -> RW for patching
        make_writable(ptr, size).expect("make_writable failed");

        // Write new data
        {
            let _write = JitWriteScope::enter();
            // SAFETY: as above; the region is writable again.
            unsafe {
                *ptr = 0xBB;
            }
        }

        // Transition back to RX
        make_executable(ptr, size).expect("make_executable failed");
        // SAFETY: an in-bounds read of a readable region.
        unsafe {
            assert_eq!(*ptr, 0xBB);
        }

        free_executable(ptr, size);
    }

    /// `Display` must carry the OS code, which is the only thing in the error
    /// that a log reader can act on.
    ///
    /// `-1` is asserted because that is what `mprotect` returns for every
    /// failure; the payload is deliberately the `errno`, and a `Display` that
    /// dropped it would leave the operator with "protect failed" and nothing
    /// else. The `AllocationFailed` half of this test went with the variant in
    /// round 10 wave 8.
    #[test]
    fn jit_error_display() {
        let e = JitError::ProtectFailed(-1);
        assert!(format!("{}", e).contains("-1"));
        assert!(format!("{}", JitError::ProtectFailed(13)).contains("13"));
    }

    /// `flush_icache_range_windows` must be callable on any Windows host.
    ///
    /// The production call site is arch-gated (x86/x86-64 have coherent
    /// caches and skip it), so on an ordinary x86-64 Windows CI machine the
    /// call site never runs and a broken `extern` declaration — wrong
    /// calling convention, wrong argument widths, missing symbol — would
    /// only surface on a Windows-on-ARM build. Calling it directly here
    /// forces the declaration to be compiled, linked and executed on every
    /// Windows host. `FlushInstructionCache` is valid (and a documented
    /// no-op) on x86-64, so this is safe to run anywhere Windows runs.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_icache_flush_is_callable() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");
        // SAFETY: in bounds of a fresh RW allocation nothing else references.
        unsafe {
            *ptr = 0x90; // one byte of "code" so the range is not untouched
        }
        flush_icache_range_windows(ptr, size);
        // Range covering a sub-page slice, and a zero-length range: both are
        // legal arguments and must not fault.
        flush_icache_range_windows(ptr, 4);
        flush_icache_range_windows(ptr, 0);
        free_executable(ptr, size);
    }

    /// `flush_icache_range_unix` must be callable on any Unix host.
    ///
    /// The mirror of `windows_icache_flush_is_callable`. On x86-64 Unix — the
    /// machine CI actually runs — this resolves to the documented no-op body,
    /// which is exactly the point: the two bodies must keep the SAME
    /// signature, so the unconditional call in `platform_make_executable`
    /// compiles on every target. When the allow-list version of this gate was
    /// in place there was no such coupling, and `aarch64-linux-android` simply
    /// got no cache maintenance at all.
    #[cfg(not(any(
        target_os = "windows",
        all(target_os = "macos", target_arch = "aarch64")
    )))]
    #[test]
    fn unix_icache_flush_is_callable_on_every_target() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");
        // SAFETY: in bounds of a fresh RW allocation nothing else references.
        unsafe {
            *ptr = 0x90; // one byte of "code" so the range is not untouched
        }
        // SAFETY: `ptr..ptr+size` is one allocation, so every range below is
        // within it or one past its end — the helper's whole contract.
        unsafe {
            flush_icache_range_unix(ptr, size);
            // A sub-page slice and a zero-length range: both legal.
            flush_icache_range_unix(ptr, 4);
            flush_icache_range_unix(ptr, 0);
        }
        free_executable(ptr, size);
    }

    /// The reader-side barrier must be callable, and cheap, on every host.
    ///
    /// It compiles to a single `ISB` on aarch64 and to nothing on x86-64, so
    /// the only thing that can break is the inline-asm constraint set — which
    /// this catches on any ARM host and which the x86 build cannot catch at
    /// all. Running it a few times also documents that it is idempotent: a
    /// caller that is unsure whether it is the publishing thread should just
    /// call it.
    #[test]
    fn the_reader_side_instruction_stream_barrier_is_callable() {
        for _ in 0..4 {
            synchronize_instruction_stream();
        }
    }

    /// The epoch gate issues one barrier per thread per publication epoch, and
    /// none at all in between.
    ///
    /// This is the property that makes the barrier affordable on the dispatch
    /// path: the ARM requirement is per newly-published instruction stream, not
    /// per call, so a dispatcher that issued an `ISB` on every compiled
    /// invocation would be paying a pipeline flush for a publication that
    /// happened once an hour ago.
    ///
    /// Run on its own thread because the gate's state is thread-local and the
    /// seed ("this thread has never synchronised") is only observable once. The
    /// counter is process-wide, so this is also the only test in the file that
    /// may touch it.
    ///
    /// On x86-64 the whole mechanism is compiled out, and since round 10 wave 8
    /// the accessor says so: every reading is `None`, not `Some(0)`.
    ///
    /// That change matters to this test more than to any caller. The previous
    /// x86-64 arm asserted `(first - start, repeats - first, publication -
    /// repeats) == (0, 0, 0)` on a `u64`, and three zero deltas are what a
    /// compiled-out gate produces *and* what a gate that ran but never
    /// incremented produces *and* what a gate whose counter was removed
    /// produces. It could not fail. `None` on all four readings is a statement
    /// about the target rather than about a delta, and it does fail if the
    /// accessor ever starts reporting a count on an architecture that cannot
    /// have one.
    #[test]
    fn the_epoch_gated_barrier_runs_once_per_publication_epoch() {
        let readings = std::thread::spawn(|| {
            let start = instruction_stream_barriers_issued();
            // Epoch 0 is a legal epoch, and a thread that has never
            // synchronised must not mistake itself for one that already has at
            // epoch 0 — which is why the seed is `u64::MAX`.
            synchronize_instruction_stream_for_epoch(0);
            let after_first = instruction_stream_barriers_issued();
            for _ in 0..8 {
                synchronize_instruction_stream_for_epoch(0);
            }
            let after_repeats = instruction_stream_barriers_issued();
            synchronize_instruction_stream_for_epoch(1);
            let after_publication = instruction_stream_barriers_issued();
            (start, after_first, after_repeats, after_publication)
        })
        .join()
        .expect("the gate must not panic");

        let (start, first, repeats, publication) = readings;
        #[cfg(not(target_arch = "x86_64"))]
        {
            // `expect` rather than `unwrap_or(0)`: a `None` here would mean the
            // accessor has decided this target has no barrier mechanism while
            // the mechanism is compiled in, and folding that into a zero is how
            // the delta assertions below would pass on a broken accessor.
            let start = start.expect("a non-x86-64 target counts its barriers");
            let first = first.expect("a non-x86-64 target counts its barriers");
            let repeats = repeats.expect("a non-x86-64 target counts its barriers");
            let publication = publication.expect("a non-x86-64 target counts its barriers");
            assert_eq!(first - start, 1, "a thread's first dispatch synchronises");
            assert_eq!(repeats - first, 0, "eight more calls at the same epoch");
            assert_eq!(publication - repeats, 1, "a new epoch synchronises again");
        }
        #[cfg(target_arch = "x86_64")]
        {
            assert_eq!(
                (start, first, repeats, publication),
                (None, None, None, None),
                "x86-64 has no reader-side barrier to count, which is a different \
                 statement from having counted zero of them — and unlike three \
                 zero deltas, this one can fail"
            );
        }
    }

    /// End-to-end check that `make_executable` flushes the icache on
    /// aarch64 so a freshly-written function actually executes.
    ///
    /// We allocate a page, write a single `RET` instruction (0xD65F03C0
    /// little-endian on aarch64 = `ret`), finalize via `make_executable`
    /// (which on every non-macOS aarch64 Unix also runs `__clear_cache`), then
    /// transmute the pointer to a `fn()` and call it. On x86-64 hosts
    /// this test is compiled out; on macOS aarch64 it exercises the
    /// existing `sys_icache_invalidate` path, which is a useful
    /// regression guard too.
    ///
    /// The cfg is "aarch64 and not Windows" rather than a list of operating
    /// systems for the same reason the production gate is: an allow-list
    /// omitted `aarch64-linux-android`, so the one target that had no flush
    /// also had no test that would have noticed.
    #[cfg(all(target_arch = "aarch64", not(target_os = "windows")))]
    #[test]
    fn aarch64_icache_flush_executes_fresh_code() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");

        // aarch64 `ret` = 0xD65F03C0, little-endian byte sequence
        // 0xC0 0x03 0x5F 0xD6.
        {
            let _write = JitWriteScope::enter();
            // SAFETY: four in-bounds bytes of a fresh allocation.
            unsafe {
                *ptr.add(0) = 0xC0;
                *ptr.add(1) = 0x03;
                *ptr.add(2) = 0x5F;
                *ptr.add(3) = 0xD6;
            }
        }

        make_executable(ptr, size).expect("make_executable failed");

        // Jump to the freshly-written code. If the icache was not
        // flushed the CPU may fetch zero bytes (UDF) or stale data
        // and trap; a clean flush makes this a no-op call that
        // returns normally.
        // SAFETY: the region now holds exactly one `RET` and is executable, so
        // it is a valid `extern "C" fn()` that returns immediately.
        let f: extern "C" fn() = unsafe { std::mem::transmute(ptr) };
        f();

        free_executable(ptr, size);
    }
}

// ---------------------------------------------------------------------------

/// The placement window, checked against the architecture rather than against
/// itself.
///
/// `mmap` hinting cannot produce a wrong address — the fallback re-maps
/// unhinted and every emitter range-checks its own displacement — so what is
/// worth testing is the ARITHMETIC that decides whether a placement was worth
/// keeping. Getting it wrong in the permissive direction costs nothing at run
/// time (the emitter refuses and falls back to the long form) and costs the
/// whole point of the feature, silently.
#[cfg(all(
    test,
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
mod near_globals_tests {
    use super::near_globals::{in_reach, nth_hint, walk, CursorUpdate, Hint, WINDOW};

    /// Both ENDS have to be in reach, not just the base. A buffer whose start
    /// is 1.4GB away and whose end is 2.1GB away is one the emitter can use
    /// for its first instruction and not for its last, which is the shape that
    /// would make engagement depend on where in the body a guard happens to
    /// land.
    #[test]
    fn a_buffer_is_in_reach_only_when_both_ends_are() {
        let anchor = 0x2000_0000_0000usize;
        assert!(in_reach(anchor + (64 << 20), 4096, anchor), "64MB away");
        assert!(
            !in_reach(anchor + WINDOW - 4096, 1 << 20, anchor),
            "base inside the window and the end outside it must be refused"
        );
        assert!(
            !in_reach(anchor + WINDOW + 1, 4096, anchor),
            "past the window"
        );
    }

    /// Below the anchor as well as above it. `abs_diff` is what makes that
    /// true, and a subtraction written the other way round would silently
    /// refuse every placement the kernel puts underneath.
    #[test]
    fn the_window_is_two_sided() {
        let anchor = 0x2000_0000_0000usize;
        assert!(in_reach(anchor - (64 << 20), 4096, anchor));
        assert!(!in_reach(anchor - WINDOW - (1 << 20), 4096, anchor));
    }

    /// The window must stay inside the architectural ±2GB `disp32` reaches,
    /// with room for the displacement being measured from the END of an
    /// instruction somewhere inside the buffer.
    #[test]
    fn the_window_is_inside_what_disp32_reaches() {
        assert!(
            WINDOW < i32::MAX as usize,
            "the placement window is wider than a RIP displacement can reach,              so a buffer this accepts can still emit the long form -- which              makes every engagement count a lottery"
        );
    }

    /// The ladder must probe BELOW the anchor, and early.
    ///
    /// This is the bug the first version shipped with, stated as a test: a
    /// walk that only goes up finds nothing on a host whose allocator reserves
    /// one 16GB arena with the anchor 511MB inside it, because everything
    /// above the anchor within reach is that arena. The free space is
    /// underneath. A ladder that reaches down only on its seventh rung would
    /// have the same failure with extra steps, so this pins the FIRST rung.
    #[test]
    fn the_ladder_probes_below_the_anchor_first() {
        let anchor = 0x2001_E810_3F0usize;
        let first = nth_hint(0, 0, 4096, anchor).addr().expect("a first hint");
        assert!(
            first < anchor,
            "the first hint is above the anchor ({first:#x} vs {anchor:#x}),              which is the side an arena reserved upward from its base occupies"
        );
        // …and the ladder must get far enough down to clear a half-gigabyte
        // arena prefix within its rungs.
        let deepest = (0..16)
            .filter_map(|i| nth_hint(i, 0, 4096, anchor).addr())
            .filter(|h| *h < anchor)
            .map(|h| anchor - h)
            .max()
            .expect("some rung below");
        assert!(
            deepest >= 1 << 30,
            "the ladder only reaches {deepest} bytes below the anchor; an              arena prefix larger than that puts every rung inside it"
        );
    }

    /// Every rung it offers is one `in_reach` would accept, so a hint is never
    /// proposed that the acceptance check would then refuse.
    #[test]
    fn every_rung_is_inside_the_window() {
        let anchor = 0x2001_E810_3F0usize;
        for i in 0..16 {
            if let Some(h) = nth_hint(i, 0, 1 << 20, anchor).addr() {
                assert!(
                    in_reach(h, 1 << 20, anchor),
                    "rung {i} proposes {h:#x}, outside the window it is checked against"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // The retirement decision.
    //
    // These five tests exist because of
    // `docs/internal/fixed-bugs/r10-readers-near-globals-place-retires-on-a-stale-cursor-FIXED-20260922.md`:
    // `nth_hint` returned `Option<usize>`, `place` `break`s on `None`, and the
    // only statement after the loop set `RETIRED` — so "this step has no address
    // for a buffer this size" and "the ladder is walked out" were the same value
    // and had the same consequence, which was switching the strategy off for the
    // life of the process.
    //
    // They drive `walk` and not `place`, and that is the point of `walk`
    // existing: `place` reads `enabled()` (a `OnceLock` over an environment
    // variable), `RETIRED` (a process-global `AtomicBool` with no reset) and
    // `CURSOR`, so from a test it can neither be armed nor observed. `walk`
    // returns the decision as a `Walk`, and `map`/`unmap` are already parameters,
    // so every assertion below is about pure arithmetic over fabricated
    // addresses. Nothing here maps a page or dereferences a pointer.
    //
    // 64-bit only: the anchors are 46-bit constants, as in the tests above.
    // -----------------------------------------------------------------------

    /// A ladder rung that has no address for THIS size is not the ladder
    /// running out, and neither is a held cursor that cannot fit THIS size.
    ///
    /// The three states are the fix's shape. `Hint::Exhausted` is the only one
    /// that means "the ladder is walked out", and before this enum existed
    /// `TooBigHere` was spelled the same way and was treated the same way.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn the_three_reasons_a_step_offers_no_address_are_distinguishable() {
        let anchor = 0x2000_0000_0000usize;

        // `LADDER` has 8 rungs, so with no cursor held step 8 is past its end.
        assert_eq!(
            nth_hint(8, 0, 4096, anchor),
            Hint::Exhausted,
            "step 8 with no cursor is one past the last rung"
        );

        // 3GiB cannot fit at ANY rung: the deepest offset is 1GiB, so the
        // nearest a 3GiB buffer's far end can come to the anchor is 3GiB - 1GiB
        // = 2GiB, and `NEAR_WINDOW` is 1.5GB. Every rung is `TooBigHere` — and
        // `TooBigHere`, not `Exhausted`, because the rung exists and a 4KiB
        // request would be handed its address.
        for i in 0..8 {
            assert_eq!(
                nth_hint(i, 0, 3 << 30, anchor),
                Hint::TooBigHere,
                "rung {i} for a 3GiB request"
            );
        }

        // A cursor whose BASE has left the window is out for every size, so it
        // is permanently absent rather than merely too small for this request.
        assert_eq!(
            nth_hint(0, anchor + WINDOW + (1 << 20), 4096, anchor),
            Hint::NoSuchAddress,
            "a cursor outside the window at its base"
        );
    }

    /// **The regression test for the filed bug.** A held cursor that cannot fit
    /// THIS allocation costs the walk its own step, and the ladder is still
    /// walked.
    ///
    /// # Why this fails on the old behaviour
    ///
    /// The cursor below has its base inside the window and its END outside it
    /// for a 1MiB request, so the pre-fix `nth_hint(0, cursor, 1MiB, anchor)`
    /// returned `None` — `in_reach(cursor, size, anchor).then_some(cursor)`. The
    /// pre-fix loop was `let Some(hint) = nth_hint(..) else { break };`, so it
    /// left the loop at step 0 having handed nothing to `mmap`, and the only
    /// statement after the loop was `RETIRED.store(true, ..)`. On that code this
    /// same input produced no placement and retired the strategy, so BOTH
    /// assertions below — `placed == Some(rung 0's address)` and
    /// `retire == false` — were false, in a way no arrangement of the map
    /// closure could change.
    ///
    /// The `map` closure here honours its hint exactly, which is the kernel
    /// behaviour the strategy is built for. It is called once, for rung 0, and
    /// never for the cursor: `probed == 1` is that claim.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_cursor_too_small_for_this_size_costs_one_step_and_not_the_strategy() {
        let anchor = 0x2000_0000_0000usize;
        let size = 1 << 20;
        // Base 4KiB inside the window; base + 1MiB is 1MiB - 4KiB outside it.
        let cursor = anchor + WINDOW - 4096;
        assert!(
            in_reach(cursor, 0, anchor),
            "the cursor's base must be inside the window, or this test is \
             exercising the permanently-stale case instead"
        );
        assert!(
            !in_reach(cursor, size, anchor),
            "the cursor must NOT fit this size, which is the whole premise"
        );
        assert_eq!(nth_hint(0, cursor, size, anchor), Hint::TooBigHere);

        let maps = std::cell::Cell::new(0usize);
        let unmaps = std::cell::Cell::new(0usize);
        let w = walk(
            size,
            cursor,
            anchor,
            |hint| {
                maps.set(maps.get() + 1);
                Some(hint)
            },
            |_| unmaps.set(unmaps.get() + 1),
        );

        // Rung 0 is `(false, 64 << 20)`: 64MiB BELOW the anchor, and 64MiB is a
        // whole multiple of the 2MiB hint alignment, so the mask does not move
        // it.
        let rung0 = anchor - (64 << 20);
        assert_eq!(
            w.placed,
            Some(rung0 as *mut u8),
            "the ladder must still be walked after the cursor step declines"
        );
        assert!(
            !w.retire,
            "a cursor that cannot fit this size is not evidence that the \
             address space has no room near the anchor"
        );
        assert_eq!(w.attempt, 1, "step 0 was the cursor; rung 0 is step 1");
        assert_eq!(maps.get(), 1, "the declined cursor must cost no syscall");
        assert_eq!(w.probed, 1, "and must not be counted as probed");
        assert_eq!(unmaps.get(), 0, "the placement is kept");
        assert!(!w.map_failed);

        // The cursor moves past the new buffer, rounded up to the 2MiB hint
        // alignment: `rung0 + 1MiB` is anchor - 63MiB, which is not a multiple
        // of 2MiB, so it rounds up to anchor - 62MiB.
        assert_eq!(
            w.cursor,
            CursorUpdate::Set(anchor - (62 << 20)),
            "a placement seeds the next packing address"
        );
    }

    /// An allocation too large for any rung must not retire the strategy, and
    /// must not cost a single syscall.
    ///
    /// This is the cursor-free half of the same defect: the pre-fix `nth_hint`
    /// returned `None` for an out-of-window RUNG too, and the same `break` fell
    /// through to the same `RETIRED.store`. So one sufficiently large buffer
    /// retired the strategy on the first rung with `cursor == 0`, and every
    /// later 64-byte lambda adapter fell back. `retire == false` below is the
    /// assertion that flips.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_request_no_rung_can_hold_retires_nothing_and_maps_nothing() {
        let anchor = 0x2000_0000_0000usize;
        let maps = std::cell::Cell::new(0usize);
        let unmaps = std::cell::Cell::new(0usize);
        let w = walk(
            3 << 30,
            0,
            anchor,
            |hint| {
                maps.set(maps.get() + 1);
                Some(hint)
            },
            |_| unmaps.set(unmaps.get() + 1),
        );
        assert_eq!(
            w.placed, None,
            "no rung can hold 3GiB inside a 1.5GB window"
        );
        assert_eq!(
            unmaps.get(),
            0,
            "nothing was mapped, so nothing can be released"
        );
        assert!(
            !w.retire,
            "no rung was probed, so this walk learned nothing about the \
             address space — a smaller buffer must still get a full walk"
        );
        assert_eq!(maps.get(), 0, "an unusable hint costs arithmetic, not mmap");
        assert_eq!(w.probed, 0);
        assert_eq!(
            w.cursor,
            CursorUpdate::Keep,
            "there was no cursor to disturb"
        );
    }

    /// Retirement still happens, and only for the reason it is written for: the
    /// whole ladder was probed and the kernel put every mapping out of reach.
    ///
    /// Without this test the fix above could be "never retire", which would pay
    /// eight `mmap`/`munmap` pairs per compile forever on a host with no room
    /// near the anchor — the cost retirement exists to avoid.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_ladder_whose_every_rung_is_relocated_out_of_reach_retires() {
        let anchor = 0x2000_0000_0000usize;
        // The measured Linux behaviour this strategy was written against: the
        // kernel ignores the hint and places the mapping in its own region,
        // ~130TB away.
        let elsewhere = 0x7A53_DBCB_8000usize as *mut u8;
        assert!(!in_reach(elsewhere as usize, 4096, anchor));
        let unmaps = std::cell::Cell::new(0usize);
        let w = walk(
            4096,
            0,
            anchor,
            |_| Some(elsewhere),
            |_| unmaps.set(unmaps.get() + 1),
        );
        assert_eq!(w.placed, None);
        assert!(w.retire, "every rung existed, was mapped, and missed");
        assert_eq!(w.probed, 8, "all eight rungs, and the report prints this");
        assert_eq!(unmaps.get(), 8, "every relocated mapping is released");
        assert!(!w.map_failed);
    }

    /// A cursor whose BASE has left the window is cleared, which is what the
    /// comment on the success path has claimed since it was written, and the
    /// ladder is still walked in full.
    ///
    /// Before the fix, that cursor produced `None` at step 0 and the walk ended
    /// there: nothing was cleared, nothing was probed, and `RETIRED` was set. So
    /// `probed == 8` below is a second statement of the same regression, from the
    /// permanently-stale side rather than the size-dependent one.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_cursor_outside_the_window_is_cleared_and_does_not_shorten_the_walk() {
        let anchor = 0x2000_0000_0000usize;
        let stale = anchor + WINDOW + (1 << 20);
        let elsewhere = 0x7A53_DBCB_8000usize as *mut u8;
        let w = walk(4096, stale, anchor, |_| Some(elsewhere), |_| {});
        assert_eq!(
            w.cursor,
            CursorUpdate::ClearIfUnchanged,
            "a cursor that can never be in reach again must not be kept"
        );
        assert_eq!(
            w.probed, 8,
            "the unusable cursor cost no syscall and all eight rungs were \
             still walked"
        );
        assert!(w.retire, "and the full walk earns the retirement");
    }
}

// ---------------------------------------------------------------------------
// REVIEW-NOTE: what a follow-up must change OUTSIDE this file
// ---------------------------------------------------------------------------
//
// STALE AS OF 2026-09-21, and the headline was the part that was false. It read:
// "the arena above is complete, tested and reachable, but NOTHING CALLS IT: no
// `ExecutableBuffer::new_in` call site exists, so with or without
// `CRATONVM_JIT_CODE_ARENA` the running VM behaves exactly as it did before."
//
// Three production `new_in` call sites exist and have for some time:
//
//     jit/src/ir_lower.rs        ExecutableBuffer::new_in(platform::shared_jit_code_arena(), capacity)
//     jit/src/aarch64_backend.rs crate::ExecutableBuffer::new_in(crate::platform::shared_jit_code_arena(), ..)
//     jit/src/lambda_adapter.rs  ExecutableBuffer::new_in(platform::shared_jit_code_arena(), code.len().max(64))
//
// So `CRATONVM_JIT_CODE_ARENA=1` DOES change what the VM does, and a reader who
// trusted this paragraph would have concluded the flag was inert and stopped
// investigating. That matters most for the one thing this note exists to
// support — REVIEW-NOTE 3's measurement protocol — because "the flag does
// nothing" and "the census is never emitted" produce the same empty log, and
// until 2026-09-21 both sentences were wrong in the same direction at once.
//
// This paragraph is left in place, corrected, rather than deleted, because the
// notes below it are still partly live and each one's status is now stated
// where it stands. The file's own warning applies to itself: "this repository
// has a documented history of comments that drifted from their code."
//
// Every item below WAS a change to another file that a follow-up had to make.
// They are listed in the order they should land; see each item for what is
// actually outstanding.
//
// ---------------------------------------------------------------------------
// REVIEW-NOTE 1 (BLOCKING, read before anything else):
//   THE ADDRESS-REUSE ORDERING OBLIGATION
// ---------------------------------------------------------------------------
//
// The arena hands the SAME address to the next allocation of the same page
// count. `jit/src/implicit_null.rs`'s module doc, "Hazard 2 -- a JIT code
// buffer is freed and its address REUSED", spells out what that costs if any
// address-keyed registry still names the range: not a crash, but "silent,
// arbitrary control flow" -- a fault in the NEW body matches the OLD body's
// recovery PC and the signal handler resumes inside a live method at an address
// that means nothing there.
//
// The obligation, stated once so it can be quoted:
//
//   *** A block may be returned to a JitCodeArena only after implicit_null's
//   *** range, the JIT method-name table, jit_code_regions and the code-events
//   *** symbol sink have all been purged of that address range.
//
// Today it is discharged by inheritance, not by construction:
//
//   `CompiledMethod::drop` body   -> implicit_null::unregister_range,
//                                    unregister_jit_method_name,
//                                    the OSR-trampoline purge
//   ...then fields drop...
//   `ExecutableBuffer::drop`      -> jit_code_regions().deregister,
//                                    code_events::retire,
//                                    then platform::free_executable
//   `free_executable`             -> free_if_arena -> JitCodeArena::release
//
// So the purges all precede the free. `JitCodeArena::release` re-checks the
// `jit_code_regions` half under `debug_assert`; the other three are invisible
// from this module.
//
// A follow-up MUST NOT:
//   * call `JitCodeArena::free` from anywhere that is not downstream of
//     `ExecutableBuffer::drop`;
//   * reorder `CompiledMethod::drop` so that a field drop precedes the purges
//     (today the drop BODY runs first; that is a language guarantee, but the
//     purges themselves could be moved out of it);
//   * adopt round-8 plan item 3 as written. See REVIEW-NOTE 5.
//
// If the arena is ever given a caller that cannot prove this ordering, the
// right answer is a quarantine: `release` pushes onto a pending list, and a
// block only reaches a free list after an explicit "all registries purged for
// generation N" step. That is not built here because it needs a quiesce point
// this module cannot see, and building half of it would be worse than building
// none.
//
// ---------------------------------------------------------------------------
// REVIEW-NOTE 2: the `jit/src/lib.rs` changes that wire the arena up
// ---------------------------------------------------------------------------
//
// (a) `JitCache` gains the arena, as the round-8 plan's item 1 asks:
//
//         pub struct JitCache {
//             ...
//             /// Per-VM code arena. See `platform::JitCodeArena`; default-off
//             /// behind `CRATONVM_JIT_CODE_ARENA`.
//             code_arena: crate::platform::JitArenaHandle,
//         }
//
//     initialised in `JitCache::new` with
//     `std::sync::Arc::new(std::sync::Mutex::new(platform::JitCodeArena::new()))`,
//     plus `pub fn code_arena(&self) -> &platform::JitArenaHandle`. A compile
//     path with no `JitCache` to hand (the lambda adapter) uses
//     `platform::shared_jit_code_arena()` instead.
//
// (b) Each `ExecutableBuffer::new(...)` on a compile path becomes
//     `ExecutableBuffer::new_in(arena, ...)`. The four production sites are:
//
//         jit/src/ir_lower.rs:18866        ExecutableBuffer::new(capacity)
//                                          -- the optimizing tier's body
//         jit/src/aarch64_backend.rs:4925  ExecutableBuffer::new(machine_code.len().max(4096))
//         jit/src/lambda_adapter.rs:478    ExecutableBuffer::new(code.len().max(64))
//         the OSR trampoline in emit_osr_trampoline
//
//     `new_in` returns `new`'s exact result when the flag is off, so each of
//     these is a mechanical substitution plus getting an arena handle into
//     scope. TEST call sites (`ir_lower.rs` x6, `lambda_adapter.rs:769`,
//     `gc/tests/phase_h_integration.rs:345`) should stay on `new`: they assert
//     about buffers, not about pooling.
//
// (c) The round-8 TODO on `JitCache` (search
//     `TODO(round-8, HIGH from round-7 jit #6)`, ~line 16669) must be rewritten
//     rather than deleted. Items 1, 2 and 4 are built (in `platform.rs`); item
//     3 is refused (REVIEW-NOTE 5); item 5 is not built but the knob it needs
//     exists (`JitCodeArena::with_region_bytes`). Whatever replaces it should
//     say so and should name an owner and a next step, which this note
//     deliberately does not invent on someone else's behalf.
//
// (d) `ExecutableBuffer`'s doc comment and its two `unsafe impl`s say the
//     buffer "uniquely owns its mapping". With `new_in` that is "uniquely owns
//     its BLOCK" -- the mapping is the arena's and outlives the buffer. The
//     Send/Sync arguments are unaffected (exclusive ownership of the bytes is
//     what they turn on), but the words are now wrong, and this repository has
//     a documented history of comments drifting from code.
//
// ---------------------------------------------------------------------------
// REVIEW-NOTE 3: measure before flipping the default
// ---------------------------------------------------------------------------
//
// Nothing here was measured; the ~177-192 MiB figure is the round-8 TODO's
// arithmetic, carried forward unverified. The evidence a default flip needs, on
// Windows, on a JDK-shaped workload, with the flag on and off:
//
//   * peak private + reserved bytes for the process;
//   * `platform::jit_code_arena_census()` at exit -- specifically
//     `served_from_free_list` (did pooling engage at all?),
//     `bytes_padding_live` (what page rounding actually cost) and
//     `served_standalone` (how often the arena declined);
//   * compile throughput, since `alloc` now takes a process-wide lock that
//     `VirtualAlloc` did not;
//   * `regions_reclaimed` and `bytes_reclaimed` -- did the arena ever get to
//     give a region back, or does the workload's peak simply never recede?
//     A zero here on a long run means reclamation is dead code for that shape
//     and the hysteresis spare is the whole of the steady-state cost;
//   * `blocks_split` against `served_from_free_list`, and
//     `bytes_free_listed / free_blocks` over time. That ratio is the only
//     evidence that would justify building coalescing: a mean free-block size
//     decaying towards one page while the free-listed bytes stay large is
//     fragmentation the splitter caused and cannot undo.
//
// One number that is NOT evidence of a saving: `bytes_reclaimed`. On Unix with
// `CRATONVM_JIT_POISON_FREE=1` the mapping is retired with `mprotect(PROT_NONE)`
// and the address space is not returned, so that counter measures bytes retired
// rather than bytes given back. Measure with the poison flag OFF.
//
// `platform::log_jit_code_arena_census` emits all of it at `info!`. It is
// `info!` and not `debug!` on purpose: the workspace `tracing` dependency sets
// `release_max_level_info`, so a `debug!` line cannot be recovered from a
// release binary with any `RUST_LOG` value.
//
// WHERE THE CALL IS, and why this paragraph exists. Until 2026-09-21 the
// sentence above was the whole of it, and it was false: nothing in the
// workspace called `log_jit_code_arena_census`, so anyone following this note
// would have set `RUST_LOG=info`, run the workload, found no `arena_id=` event,
// and had to write the call before any of the measurement above could start.
// `rg -nw 'log_jit_code_arena_census'` returned the definition, this sentence,
// and one line of `docs/jit/code-cache-lifecycle.md` — no call site
// (`docs/internal/fixed-bugs/r10-gauges-arena-census-logger-has-no-caller-FIXED-20260922.md`).
// The call now lives in `vm-cli/src/main.rs`'s `maybe_dump_shutdown_reports`,
// behind `platform::jit_code_arena_enabled()`, so the protocol is:
//
//     CRATONVM_JIT_CODE_ARENA=1 RUST_LOG=info cratonvm <workload>
//
// and the census arrives as the `jit code arena census` event plus the two
// shape events and any pathology `warn!`s. With the flag unset the line is not
// reached at all, which is why a default run's log is unchanged.
//
// THAT PROTOCOL DID NOT WORK UNTIL 2026-09-22, and the reason is worth keeping
// here because it is the third distinct way this one measurement has come back
// empty. `vm-cli` built its subscriber as
// `EnvFilter::from_default_env().add_directive(Level::WARN.into())`. Both
// `RUST_LOG=info` and that floor parse to TARGETLESS directives, and `EnvFilter`
// resolves a same-target tie by last-added-wins — so the floor silently
// downgraded a bare `RUST_LOG=info` back to WARN, and the line above produced
// ZERO info events on any target. Measured, not read: the census was recovered
// with `RUST_LOG=cratonvm_jit=info` on the same binary that emitted nothing with
// `RUST_LOG=info`. `vm-cli` now adds the floor only when `RUST_LOG` names no
// global level of its own, so the protocol as written above is what to run. If
// it ever comes back empty again, check that first — it is cheaper to rule out
// than the arena.
//
// Two caveats on reading a MISSING census, both of which look identical to the
// regression this paragraph documents:
//   * `jit_code_arena_enabled()` is hard-`false` where `JIT_CODE_ARENA_IS_INERT`
//     (macOS/ARM64), so no amount of flag-setting produces a census there. That
//     is correct — an inert arena serves every request standalone and there is
//     no pooling to measure — and it is one more reason this note asks for the
//     measurement on Windows.
//   * `maybe_dump_shutdown_reports` latches on a `DUMPED` compare-exchange and
//     runs once per process. A run that terminates without reaching it (a hard
//     `abort`, a crash in the VM's own teardown) emits no census, and that is
//     an absent measurement rather than an arena that did nothing.
//
// ---------------------------------------------------------------------------
// REVIEW-NOTE 4: degradations that address reuse introduces
// ---------------------------------------------------------------------------
//
// Two diagnostics assume an address is never recycled, and become weaker (not
// wrong) once it is. Neither is a reason to hold the arena back; both should be
// noted where they live.
//
//   * `RECENT_FREE_BASE` / `recent_code_free_covering` in `jit/src/lib.rs` --
//     the crash handler's "was this PC inside a buffer we had just released?"
//     ring. With reuse, a PC inside a healthy NEW body can match a ring entry
//     for the OLD one, so the answer becomes "this address was recently freed",
//     which is true and no longer implies a use-after-free. Suggested fix:
//     stamp ring entries with `JIT_INSTALL_EPOCH` and report the epoch.
//   * `CRATONVM_JIT_POISON_FREE` (this file) is inert for arena blocks: it
//     poisons whole mappings with `mprotect(PROT_NONE)`, and an arena block is
//     not a mapping. A run that needs it should set `CRATONVM_JIT_CODE_ARENA=0`;
//     the two flags answer opposite questions and there is no reason to make
//     them compose. UPDATE (region reclamation): it is no longer *entirely*
//     inert. `reclaim_region` calls `platform_free` on a whole region, so with
//     the poison flag set a reclaimed region is retired as PROT_NONE and its
//     address space is never reused -- which is the flag's intent, at region
//     granularity instead of block granularity. The consequence for a
//     measurement run is in REVIEW-NOTE 3: `bytes_reclaimed` then counts bytes
//     retired, not bytes returned.
//   * `CRATONVM_JIT_NEVER_FREE_CODE` (`jit/src/lib.rs`) returns from
//     `ExecutableBuffer::drop` BEFORE `free_executable`, so with it set no
//     block ever comes back to the arena, no region ever goes empty, and
//     `regions_reclaimed` is necessarily zero. That is correct behaviour for a
//     leak-everything diagnostic and is stated here only so that a zero in a
//     census taken under that flag is not read as a reclamation bug.
//
// ---------------------------------------------------------------------------
// REVIEW-NOTE 5: round-8 plan item 3 is WRONG and must not be implemented
// ---------------------------------------------------------------------------
//
// The plan says the JIT-code-region tracker should "register whole *arenas*
// instead of individual buffers -- the validator uses a single range check per
// arena". It must not. `jit_code_regions` is what `validate_code_ptr` consults
// before transmuting a `*const u8` into a function pointer, and its job is to
// answer "is this a LIVE code buffer?". A whole-arena range answers "is this
// inside 16 MiB we once mapped?", which is `true` for a freed block, for a
// block sitting on a free list, and for the region's never-bumped tail. That
// converts a caught bad pointer into an executed one.
//
// `ExecutableBuffer::new_in` therefore registers the individual block, exactly
// as `new` registers the individual mapping, and `Drop` deregisters it, exactly
// as it does today. The per-arena range check the plan wanted is a legitimate
// FAST PATH -- "not in any arena span, so certainly not arena code" -- but it
// is a pre-filter, never the answer.

// ---------------------------------------------------------------------------
// REVIEW-NOTE 6: the process-globals ratchet has to move, and owes a paragraph
// ---------------------------------------------------------------------------
//
// `jit/tests/process_global_statics_ratchet.rs` pins the number of `static`
// declaration lines under `jit/src` EXACTLY: it fails when the count exceeds
// `BASELINE` and also when it falls below (so a removal cannot pass
// unrecorded). This file's arena adds SIX, all in `platform.rs`, and the
// ratchet's own rule is that a raise must be argued for rather than made room
// for. The argument, in the form that file asks for:
//
//   * `PAGE` (in `code_page_size`) -- an `OnceLock` cache of a value that is a
//     property of the CPU and the kernel, not of a VM. The ratchet's failure
//     message names "a cache of an environment flag" as the category this is
//     in; this is the same thing one step further out, and querying the page
//     size per allocation would be a syscall on the compile path.
//   * `ON` (in `jit_code_arena_enabled`) -- an `OnceLock` cache of an
//     environment flag, verbatim the carved-out category, and the same shape as
//     `near_globals::enabled` and `jit_poison_free_enabled` two screens up in
//     this same file.
//   * `ARENA_SPANS` and `ARENA_SPAN_COUNT` -- the address-to-arena table that
//     lets `free_executable` route a block back to its arena. Process-global by
//     necessity, not by convenience: `ExecutableBuffer::drop` has a raw pointer
//     and nothing else, and this change may not give it a field (REVIEW-NOTE
//     2). The count is separate from the table because the free path must be
//     able to answer "no arena exists" without taking a lock, which is the
//     whole of its cost when the flag is off. They are ONE structure in two
//     declarations, like `implicit_null.rs`'s parallel arrays.
//   * `NEXT_ARENA_ID` -- a monotonic counter, process-wide because arena
//     identities must not be recycled across the whole span table. See
//     `JitCodeArena::id` for what goes wrong with a per-arena or
//     address-derived one.
//   * `ARENA` (in `shared_jit_code_arena`) -- the process-wide arena itself.
//     This is the only one that is genuinely shared STATE rather than a cache
//     or an index, and it is the one a reviewer should push back on. It exists
//     so that a compile path with no `JitCache` in scope (the lambda adapter)
//     has an arena at all. The per-VM answer is REVIEW-NOTE 2(a): once
//     `JitCache` owns a `JitArenaHandle`, every method-body compile uses that
//     one and this singleton is left serving only the handle-less callers. If
//     a follow-up gets an arena to those callers too, DELETE it and lower the
//     baseline.
//
// The number: DO NOT ACT ON THE ONE THIS NOTE USED TO GIVE. It said "`jit/src`
// declares 721 statics by the ratchet's own grep. So `BASELINE` becomes 721."
// That was true of the worktree this note was written on and has been stale for
// some time: `jit/tests/process_global_statics_ratchet.rs` now reads
// `const BASELINE: usize = 796;`. Setting it to 721 today would make that test
// RED, and in the loudest way available, because the ratchet panics in BOTH
// directions -- `count > BASELINE` for a new static and `count < BASELINE` for a
// removed one. A stale absolute count in a note that tells someone to go and
// write it into a gate is worse than no number, which is why this paragraph now
// gives none.
//
// The procedure, not the number: take the count on the merged tree with the grep
// below -- which is the ratchet's own -- and move the constant to match, in a
// commit that says which statics moved and why.
//
// Take it on the MERGED tree, not on a lane's worktree, and the original note
// here is the evidence for why: it recorded that this branch already stood at
// 715 against a `BASELINE` of 720 while that work was in flight, i.e. the ratchet
// was already red in the "count is below the baseline" direction from somebody
// else's concurrent edits. Several lanes add and remove statics in `jit/src` in
// one round, so any count taken before the merge is a count of a tree that will
// not exist.
//
//     grep -rhE '^\s*(pub(\([^)]*\))?\s+)?static\s+(mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:' \
//         jit/src --include=*.rs | wc -l
//
// ---------------------------------------------------------------------------
// REVIEW-NOTE 7: the flag is already declared -- do not declare it twice
// ---------------------------------------------------------------------------
//
// `CRATONVM_JIT_CODE_ARENA` is ALREADY registered on this branch, in all three
// places the flag-surface gate requires, dated 2026-09-16:
//
//     types/src/flag_groups.rs:1402   E { group: Group::JIT, token: "code-arena",
//                                         on_key: Some("CRATONVM_JIT_CODE_ARENA"), ... }
//     types/tests/flag-surface.txt:727
//     docs/config/flag-inventory.md:1396
//
// So `types/tests/flag_surface.rs` and `tools/flag-census/check-surface.sh`
// are satisfied by the literal this file now reads, and nothing further is
// needed. A second INVENTORY row would fail `flag_declaration_guard.rs`.
//
// Because the flag is declared, `runtime_flag_on` resolves it through the
// grouped reader, so `CRATONVM_JIT=code-arena` works as well as
// `CRATONVM_JIT_CODE_ARENA=1`. The doc on `jit_code_arena_enabled` says so;
// keep the two in step if the token is ever renamed.
//
// The reclamation/splitting/eviction round adds NO statics. The one new
// item-level declaration is `const JIT_CODE_ARENA_EMPTY_REGION_SPARES`, and the
// ratchet's grep matches `static`, not `const`, so `BASELINE` is unchanged by
// it. Everything else the round added is a struct field or a method.
//
// ---------------------------------------------------------------------------
// REVIEW-NOTE 8: the eviction seam, and what a POLICY would have to key on
// ---------------------------------------------------------------------------
//
// `JitCodeArena::set_eviction_hook` is a seam. It is unset by default, NOTHING
// in this workspace installs one, and an arena with no hook behaves exactly as
// it did before the hook existed -- so the census fields `evictions_requested`
// and `evictions_performed` are zero on every run today, by construction. It is
// not a feature and this note is not a claim that eviction is built.
//
// Two things a follow-up must do before it is one.
//
// (a) MOVE THE CALL OUT OF THE LOCK. `alloc` invokes the hook while holding the
//     arena mutex, so a hook that does the obvious thing -- drop a cold
//     `CompiledMethod` -- deadlocks: `CompiledMethod::drop` -> field drop ->
//     `ExecutableBuffer::drop` (lib.rs:1219) -> `platform::free_executable` ->
//     `free_if_arena` -> `handle.lock()`, on a `std::sync::Mutex` that is not
//     reentrant. Today a hook may only SELECT and queue. `ExecutableBuffer::
//     new_in` already brackets the lock explicitly, so the shape is: try an
//     allocation that is forbidden to grow, drop the guard, ask, re-take the
//     guard and allocate for real. That splits `alloc` in two and the tests in
//     this file pin `alloc`'s current contract, which is why it is a follow-up
//     and not this change.
//
// (b) KEY IT ON DATA THAT EXISTS. I went and read it. What exists is thinner
//     than "LRU eviction keyed by invocation count" implies:
//
//     * `cratonvm_jit::tiered::MethodState` (`jit/src/tiered.rs:493`) has
//       `invocation_count: u64` (:499) and `backedge_count: u64` (:502). They
//       are PLAIN `u64`, not atomics -- the whole struct lives in
//       `CompilerCore::methods: Mutex<FxHashMap<MethodKey, MethodState>>`
//       (`tiered.rs:990`), keyed by `MethodKey` (`tiered.rs:435`:
//       `class_id`/`class_name`/`method_name`/`descriptor`). They are written
//       under that mutex by `on_method_invocation*` (`tiered.rs:2710`) and
//       `request_osr` (:2783) with `saturating_add`, and the only public reader
//       is `method_states()` (:3140), which CLONES THE WHOLE MAP under the
//       lock. A policy that consulted it per allocation would be a full map
//       clone on the compile path; it needs a cheaper accessor, which does not
//       exist yet.
//     * There is NO RECENCY SIGNAL ANYWHERE. `MethodState` has no last-used
//       timestamp; `last_compile_time_ms` (:535) is a DURATION, and
//       `trap_decay_ms` (:530) records the last trap decay, not the last call.
//       `CompiledMethod` has no hit count and no timestamp either. The
//       invocation counters are monotonic and are never decayed (only
//       `deopt_count`/`trap_counts` decay, `tiered.rs:699`). So "LRU" is not
//       implementable from what is in the tree: what IS implementable is
//       "least-invoked", which is a different and much worse policy -- a method
//       that ran a million times an hour ago outranks one that ran a thousand
//       times a second ago, forever. Adding recency means adding a field and
//       paying for its update on the invocation path, and that cost is the
//       first thing a policy proposal has to justify.
//     * The bytes a policy would be trying to reclaim are tracked ONLY
//       process-globally, in `COMMITTED_JIT_CODE_BYTES` (`lib.rs:1108`), which
//       only decreases in `ExecutableBuffer::drop` (`lib.rs:1232`).
//       `JitCache` itself (`lib.rs:16788`) carries no byte accounting and no
//       per-entry usage data at all: it is 64 shards of
//       `ArcSwap<FxHashMap<u64, (JitKey, Arc<CompiledMethod>)>>` plus an OSR map
//       each. Note also that `JitKey` (`lib.rs:16600`) is a DIFFERENT key type
//       from `tiered::MethodKey`, so a policy has to translate between the
//       counter map and the body map.
//     * Eviction must go through `JitCache::invalidate_matching`
//       (`lib.rs:18958`) -- the one removal path -- and inherits its whole
//       protocol: the transitive reverse closure over `_direct_callee_entries`
//       and `lambda_adapter::adapters_reaching`, setting `retired` before any
//       IC is touched, and retirement through `defer_jit_owner`'s quiescence
//       queue rather than an immediate free. `vm/src/jit/helpers.rs:25010` is
//       the existing caller that most resembles an eviction (the deopt path),
//       and the comment above it names the coupling a policy inherits: the
//       tiered manager's tier demotion has to happen exactly when the body is
//       removed.
//     * And `Arc<CompiledMethod>` is not enough to free anything. Bodies hold
//       `_direct_callee_roots: Vec<Arc<CompiledMethod>>` (`lib.rs:3367`), so
//       evicting a callee frees nothing while a caller is still installed. An
//       eviction policy that counts bytes freed by "number of methods evicted"
//       will be wrong by the inlining graph.
//
//     Which is why the policy is not invented here. `jit_code_cache_at_capacity`
//     (`lib.rs:1174`) is today's answer to a full code cache and it does not
//     evict: it refuses the compile and the method stays interpreted. Changing
//     that to "evict instead of refuse" is a VM-level decision with a VM-level
//     blast radius, and `platform.rs` has no business making it.
//
// ---------------------------------------------------------------------------
// REVIEW-NOTE 9: text in `jit/src/lib.rs` that this round makes STALE
// ---------------------------------------------------------------------------
//
// The `CompiledMethod` doc section "# The code arena (round-8 "jit #6")"
// (`jit/src/lib.rs:16685`) describes the arena as it was before reclamation and
// splitting. Two passages are now false and one is now incomplete. This file
// may not edit `lib.rs`; whoever next can, should:
//
//   * item 2 (`lib.rs:16700`) says "**Exact fit only -- no splitting, no
//     coalescing**". Splitting IS built: a request is served from the smallest
//     free class at or above it, and the remainder is pushed back into its own
//     class. The accurate sentence is "splitting, but no coalescing", and the
//     limit that remains is that freed neighbours are never merged, which
//     `freed_neighbours_are_not_coalesced` in this file pins.
//   * the paragraph beginning "**LRU eviction keyed by invocation count is not
//     built either.**" (`lib.rs:16725`) is still TRUE of the policy and should
//     stay, but it now understates what exists: there is a seam
//     (`JitCodeArena::set_eviction_hook`) with no installer, and a pressure
//     report (`JitCodeArena::pressure`) for a policy to read. REVIEW-NOTE 8
//     above says what a policy would still need; the lib.rs paragraph should
//     point at it rather than repeat it.
//   * nothing in that section mentions that regions are now UNMAPPED when they
//     go empty, or that one empty region is kept as a spare. The
//     address-space story the section tells -- "reserved about 177 MB it never
//     used" -- now has a second half: the arena can give regions back, down to
//     one spare per arena, which at 16 MiB regions is the floor a quiesced
//     process settles at.
//
// None of these change behaviour; all three are the kind of comment drift this
// repository has a documented history of, which is why they are listed
// individually rather than as "update the doc".
//
// ---------------------------------------------------------------------------
// REVIEW-NOTE 10: a BUG the arena had, fixed here, that a reviewer should see
// ---------------------------------------------------------------------------
//
// `JitCodeArena::release` now calls `make_writable` on every block before it
// goes on a free list. That is not new bookkeeping; it fixes a fault the arena
// would have taken on its FIRST recycled compile, and it was found while
// building the splitting above rather than by anything failing.
//
// `ExecutableBuffer`'s lifecycle is `new` (RW) -> write -> `finalize` (RX) ->
// ... -> `drop`. `Drop` (`jit/src/lib.rs:1219`) deregisters, retires symbols,
// subtracts the committed bytes and frees — it never calls `make_writable`, and
// before the arena it had no reason to, because the mapping went back to the OS
// and the next `ExecutableBuffer::new` got fresh `PAGE_READWRITE` /
// `PROT_READ|PROT_WRITE` pages. An arena block does NOT go back to the OS. It
// sat on a free list still RX from the `finalize` of the method that had just
// died, and the next compile to pop it would have taken an access violation on
// its first `emit`. None of the existing arena tests wrote into a recycled
// block, which is why nothing caught it. There is one now:
// `a_recycled_block_is_writable_again_after_its_predecessor_was_executable`
// finalizes a block, frees it, re-allocates the same page class, asserts the
// address really is the recycled one, and then writes a byte into it. It is
// the only arena test that regresses as a fault rather than as an assertion,
// which is why the address check comes before the write.
//
// Two knock-on effects, both improvements, both stated at the call site:
//   * a freed block is no longer executable, so a stale jump into a retired
//     body faults instead of running retired instructions;
//   * the cost is one `VirtualProtect` / `mprotect` per method retirement, on
//     the retirement path. If that ever shows up in a teardown profile, the
//     answer is to skip the flip for a buffer that was never finalized — but
//     `ExecutableBuffer` does not expose that, and guessing it here would be
//     exactly the kind of "probably RW already" reasoning that produced the
//     bug.

// ---------------------------------------------------------------------------
// REVIEW-NOTE 11: this file's dead PUBLIC surface, adjudicated
// ---------------------------------------------------------------------------
//
// Round 10 wave 6 (lane `readers`) enumerated the items in this file that exist
// and are reachable from nothing, and filed them as
// `docs/internal/fixed-bugs/r10-readers-platform-dead-public-surface-RESOLVED-20260922.md`.
// Wave 7 closed one and added two. Wave 8 worked the rest, and the verdicts are
// recorded HERE as well as on the page, because a page is read by whoever went
// looking for it and this file is read by whoever is about to add a seventh
// accessor to it.
//
// The shape matters more than any one item. Every entry is a variation on ONE
// defect the round found in ten unrelated files: an instrument, API or code path
// with no production caller, reading zero forever or never firing. Zero is
// indistinguishable from "this never happened", which is why the population is
// worth tracking even where each individual item is harmless.
//
// CLOSED by deletion (wave 8):
//
//   * `JitCodeArena::is_inert` — a `pub` accessor for a field that is only ever
//     initialised from the compile-time constant `JIT_CODE_ARENA_IS_INERT`,
//     which is what every production reader reads. No caller anywhere, not even
//     a test. The FIELD stays; `alloc` reads it.
//   * `JitError::AllocationFailed` — a variant no code in the workspace could
//     construct, because the allocation door (`alloc_executable`) answers
//     `Option<*mut u8>` and has no `JitError` to return. Its only two
//     constructions were the two tests asserting properties of it. Allocation
//     failure already has an instrument that DOES fire:
//     `note_jit_code_alloc_failure(size, code_alloc_failure::OS_REFUSED)`, three
//     hundred lines above. If the variant is ever wanted back, it has to agree
//     with that accounting instead of becoming a second, quieter one.
//
// CHANGED, not closed (wave 8):
//
//   * `instruction_stream_barriers_issued` now returns `Option<u64>`. It still
//     has no production caller and remains a frozen entry in
//     `scripts/baselines/orphan-instruments-allowlist.txt`; a return type is not
//     a reader and this does not retire the entry. What it removes is the reason
//     wave 6 gave for NOT writing the reader: on x86-64 the old `u64` returned
//     `0`, which reads identically to "the gate is working", "the counter is
//     dead" and "this architecture has no such barrier". `None` says the third
//     of those in the type, so the remaining work — one line in `vm-cli`'s
//     method-stats dump — no longer requires the writer to hold an opinion about
//     which architectures have incoherent I-caches. That opinion now lives here.
//
// KEPT, with the reason (wave 8), so a later sweep does not take them as easy
// retirements:
//
//   * The eviction seam — `set_eviction_hook`, `take_eviction_hook`,
//     `has_eviction_hook`, `pressure()`, `JitCodeArenaPressure`. Installed by
//     nothing, and REVIEW-NOTE 8 above is a written argument for the shape a
//     policy would have to take before anything installs it. A seam with a
//     written rationale and no installer is a DIFFERENT thing from an instrument
//     that reads zero and implies otherwise: nobody reads the hook and concludes
//     no eviction happened. Deleting it would delete the design note with it.
//   * `code_page_size`, `CODE_ADJACENT_CELL_SIZE`,
//     `JIT_CODE_ARENA_EMPTY_REGION_SPARES`, `JitCodeArena::region_bytes`, and
//     `JIT_CODE_ARENA_IS_INERT` itself. Each is `pub` with no reference outside
//     this file, and each is genuinely used INSIDE it by code that would read it
//     identically at `pub(crate)`. So the defect is over-wide visibility, not
//     dead code, and narrowing five declarations across a crate boundary in the
//     same wave that three other lanes hold files consuming this crate's surface
//     buys tidiness at the price of a merge conflict in somebody else's diff.
//     Listed so that nobody reads `CODE_ADJACENT_CELL_SIZE`'s `pub` as evidence
//     that a caller outside this crate is expected to know the cell size.
//
// AND THE THING THE RATCHET CANNOT TELL YOU, restated because a green
// `scripts/check-orphan-instruments.sh` on this file means almost nothing:
// its census of this file is ONE candidate (`near_globals::stats`), classified
// as called by a name collision with `rec.stats()`/`pool.stats()` elsewhere in
// the tree. Its C2 rule needs a zero-argument `pub fn` returning an integer,
// tuple of integers, `Option<int>` or `Vec<(..)>`, so a `-> bool` predicate
// (`jit_poison_free_enabled`), a `-> ()` barrier
// (`synchronize_instruction_stream`), a `&self` accessor (`region_bytes`,
// `is_inert`) and every `pub const` in this file are all structurally invisible
// to it. Everything on the page above had to be found by reading. That is not a
// criticism of the gate — `docs/ci/orphan-instrument-gate.md` states all of it —
// it is the reason the reading still has to happen.
