# Code Review: native-builtins registration core (nb-core)

Reviewer: Fable (Opus 4.8) — 2026-06-10
Scope: `native-builtins/src/lib.rs` (~38.4k LOC), `native-builtins/src/phases_early.rs` (~15.4k LOC), `native-builtins/src/phases_late.rs` (~43.8k LOC). ~97.6k LOC total.

## Summary

These three files are the phased registration core for thousands of native methods. The overall engineering posture is good: a deliberate `NativeKind` tagging system (Bridge / Intrinsic / SyntheticStub) is applied via `set_category`/`with_category`; the policy-forbidden "fake main" app shims are correctly gated out of the default build behind the `app-stubs` feature (`register_app_stubs`, lib.rs:884, `#[cfg(feature = "app-stubs")]`); `obj_arg` (lib.rs:10765) returns a Java NPE instead of panicking; the four `unsafe` blocks are all narrow OS-FFI calls (RtlGenRandom, K32GetProcessMemoryInfo, getrusage) written correctly with proper struct sizing and zeroed init; and `SecureRandom` now draws from OS entropy (lib.rs:31382) rather than a predictable PRNG.

The main concerns are (1) a class of unchecked negative-index conversions in `BitSet` mutators that can wrap to a gigantic `usize` and attempt a massive allocation / panic, where real JDK throws `IndexOutOfBoundsException`; (2) several `NativeKind::Bridge`-tagged surfaces that are actually synthetic stubs returning fake values (FFM `MemorySegment` reads return 0 / writes are no-ops; `Scanner(InputStream)` discards stdin; `SubmissionPublisher`/Flow never delivers to subscribers; `ObjectInputStream.readObject` handles only TC_NULL/TC_STRING); and (3) numerous spec divergences where synthetic models silently differ from the JDK (Process model always "finished", `MessageFormat` ignores typed format elements, `essential_quarkus_locale_convert` maps `"all"` to the system default locale instead of `Locale.ROOT`).

None of the `unsafe` FFI is exploitable as written. The serialization path is *safer* than real Java because it never resolves/instantiates arbitrary classes (no gadget-chain surface) — but it is functionally a stub.

## Bugs

### B1 (high) — BitSet mutators: negative index wraps to huge usize → OOM-attempt / panic
File: `phases_early.rs:2902` (`native_bs_set`), 2912 (`set(IZ)`), 2926 (`set(II)`), 2965 (`clear(II)`), 2994 (`flip(I)`), 3005 (`flip(II)`), 2942 (`clear(I)`).
`let bit = match args.get(1) { Some(Value::Int(n)) => *n as usize, ... }` converts a negative `int` to a near-`u64::MAX` `usize`. That flows into `bs_set_bit` → `bs_ensure_capacity` (phases_early.rs:2825) which computes `let needed = ((bit_index + 64) / 64) * 64;`. With `bit_index ≈ usize::MAX`, `bit_index + 64` overflows (panic in debug builds; wraps in release), and `bs_word_count(needed)` then drives `ctx.new_array(Long, new_len)` toward a gigantic allocation. Real JDK `BitSet.set/flip/clear` throw `IndexOutOfBoundsException` for negative indices. The read-side methods (`get`, `nextSetBit`, `nextClearBit`, `previousSetBit`) DO clamp with `.max(0)` (e.g. 3079, 3061), so the asymmetry is specifically in the mutators. Reachable from any app calling `bitset.set(negativeInt)`.
Fix: reject `n < 0` with `IndexOutOfBoundsException` in every mutator (mirror the read-side `.max(0)` only where JDK is lenient, which it is not for these).

### B2 (medium) — `essential_quarkus_locale_convert("all")` returns system default locale, not Locale.ROOT
File: `lib.rs:284`. The comment says `// Locale.ROOT` but the code calls `Locale.getDefault()`, which returns the host's default locale (e.g. `en_US`), not the empty `ROOT` locale. Any caller relying on `"all"`→ROOT semantics gets wrong language/country.
Fix: return `Locale.ROOT` (read the static field) instead of `getDefault()`.

### B3 (medium) — Scanner(InputStream) silently discards all input
File: `phases_early.rs:1992`. `new Scanner(System.in)` (or any InputStream) stores an empty string ("For stdin, we store an empty string"). Every subsequent `hasNext()/next()/nextLine()/nextInt()` sees no data. Apps that read interactive or piped stdin via `Scanner` get nothing — a silent functional stub, not an error.
Fix: actually drain the InputStream (route `read()`/`readAllBytes()` through `invoke_virtual`) into the buffer, or throw NotImplemented so the gap is visible.

### B4 (medium) — String byte-index slicing in Scanner can panic on multi-byte UTF-8
File: `phases_early.rs:2039, 2063, 2076, 2099` etc. `&source[pos.min(source.len())..]` slices a `String` at a stored byte offset `pos`. `pos` is advanced from `find(char::is_whitespace)`/`find('\n')` byte offsets, which are char boundaries for ASCII delimiters, but the code never calls `is_char_boundary`. If a non-ASCII token is followed by manipulation that leaves `pos` mid-codepoint, the slice panics (`byte index N is not a char boundary`). Lower likelihood given current flows but fragile.
Fix: guard with `is_char_boundary` or operate on `char_indices`.

### B5 (medium) — Future/CompletableFuture: executor argument ignored, eager synchronous execution
File: `phases_late.rs:505` (`supplyAsync(Supplier,Executor)`), 551 (`runAsync(Runnable,Executor)`), 428 (`Future.get(timeout)`). The `Executor` argument is explicitly ignored ("Executor argument ignored — execute eagerly") and the task runs synchronously on the calling thread. `Future.get(long,TimeUnit)` never times out (line 428 ignores the timeout). This breaks any code that depends on async scheduling, custom executors, thread affinity, or timeout semantics, and can deadlock differently than the JDK (a supplier that blocks on another future runs inline instead of on a pool thread).
Fix: route through the real virtual/carrier scheduler; honor the timeout overload.

### B6 (medium) — SubmissionPublisher.submit/offer never delivers to subscribers, returns hardcoded 1
File: `phases_late.rs:16155` (ctor sets subscribers list to null), 16162 (`submit` stores only the last item and returns `Value::Int(1)` "estimated lag"), 16167 (`offer` same). A real `SubmissionPublisher` fans an item out to all registered subscribers and returns the estimated maximum lag. Here items are dropped (only `lastItem` is retained) and no subscriber's `onNext` is ever called. Any reactive-streams / Flow consumer silently receives nothing.
Fix: maintain the subscriber list and invoke `onNext`, or throw NotImplemented.

### B7 (low) — Charset.forName accepts any non-empty name, never throws UnsupportedCharsetException
File: `phases_late.rs:226`. After `normalize_charset_name`, any non-empty string yields a synthetic Charset. Real JDK throws `UnsupportedCharsetException` for unknown charsets; code that catches it to fall back will not behave correctly.

### B8 (low) — MessageFormat ignores typed format elements
File: `phases_early.rs:7555` (`p52_message_format_apply`). Only the argument index of `{n,...}` is honored; the `number`/`date`/`time`/`choice` sub-format and any pattern (`{0,number,#.##}`) are discarded — the raw value is substituted via `p52_format_value`. Also `p52_format_value` (7613) only `read_string`s actual String objects; other reference types render as `"null"` instead of calling `toString()`. Produces wrong localized/typed output.

## Vulnerabilities

No memory-safety vulnerabilities were found in the reviewed code. The `unsafe` blocks are sound (lib.rs:31398 RtlGenRandom over `chunks_mut(u32::MAX)`; phases_late.rs:12233/12259 OS memory-info FFI with correctly-sized zeroed structs). Findings below are availability/robustness, not exploitable memory corruption.

### V1 (medium) — Unbounded allocation / panic via untrusted BitSet index (availability/DoS)
Same root cause as B1. An attacker-controlled `int` passed to `BitSet.set/flip/clear` (e.g. derived from parsed network/classfile data) drives a wrapping add and a multi-gigabyte array allocation attempt. This is the most concrete DoS surface in scope. Classified separately because it is reachable from untrusted lengths/indices, which the task brief calls out explicitly.

### V2 (low) — Latent panic on short args via direct `args[1]` indexing
Many natives index `args[1]`/`args[2]` directly instead of via `obj_arg`/`args.get` (e.g. Scanner ctor `phases_early.rs:2006`; MessageFormat `7510`, `7527`). If the dispatcher ever invokes a native with a shorter slice than its descriptor implies (malformed call, descriptor/registration mismatch), this panics with index-out-of-bounds (an abort, not a Java exception). The descriptor normally guarantees arity, so this is latent rather than directly reachable, but it is a robustness gap given the file handles untrusted classfiles.

## Stubs and Unimplemented

The following are `NativeKind::Bridge`-tagged but behave as synthetic stubs returning fake values — per project policy these should either implement real behavior or throw `NotImplemented` (via `native_not_implemented`, lib.rs:10796) so the gap is visible rather than silently faked.

- S1 (Foreign Function & Memory API) — `phases_late.rs:25941+`. `MemorySegment.get(...)` returns `0` (26053/26059/26065); `set(...)` is `native_noop_with_this` (26071/26077/26083); `address()` always 0; `allocate` never allocates real memory. Any app using off-heap FFM data reads zeros / drops writes silently.
- S2 (Process model) — `phases_late.rs:7202+`. `isAlive()` always false (7213); `waitFor(timeout)` always true (7302); `getOutputStream()` returns a no-op OutputStream (7286). Consistent with the synchronous synthetic-process model but shadows real process semantics.
- S3 (java.io.ObjectInputStream.readObject) — `phases_late.rs:34287`. Handles only TC_NULL and TC_STRING; every other type tag returns null. `defaultReadObject()` (34440) is a no-op. Real object graphs do not deserialize. (Note: this is *safe* — it never resolves arbitrary classes, so no deserialization-gadget RCE surface — but it is a functional stub.)
- S4 (Flow / SubmissionPublisher) — see B6.
- S5 (Scanner(InputStream)) — see B3.
- S6 (Gatherer API) — `phases_late.rs:25195` ("Stub for the Gatherer API").
- S7 (Template processor / STR) — `phases_late.rs:26329` ("Stub for template processor API").
- S8 (PosixFilePermissions.asFileAttribute) — `phases_late.rs:34069` returns null `FileAttribute` (documented as a fallback bridge; acceptable).
- S9 (ForkJoinPool common pool) — `phases_late.rs:42874` forces parallelism=1; `getActiveThreadCount()` always 0 (42913). Documented test-determinism limitation; the real carrier pool lives elsewhere.
- S10 (Process.getOutputStream / ProcessHandle) — `phases_late.rs:7306+` ProcessHandle is a stub.
- S11 (CompletableFuture.allOf/anyOf) — `phases_late.rs:572/603` synthesize a completed CF from already-eager constituents; no real composition / dependency wait.
- S12 (FileSystem.isOpen) — `phases_late.rs:4276` always returns 1 ("default FS is always open").
- S13 (charset decoder no-op) — `phases_late.rs:6319` ("Stub — not critical for bootstrap").

The default-build app shims under `register_app_stubs` (lib.rs:884) are correctly `#[cfg(feature = "app-stubs")]`-gated and `NativeKind::SyntheticStub`-tagged, and the unimplemented fallback is `native_not_implemented` — this part of the policy is being honored.

## Performance

- P1 — `format!("{:02x}", b)` allocated per byte in `HexFormat.formatHex` hot loop (`lib.rs:6639`, also 5779-area `formatHex([B)`). For large byte arrays (digests, certs) this is one `String` allocation per byte. Replace with a direct nibble-to-hex push into the preallocated `String` (lookup table or `write!` into a buffer).
- P2 — `BitSet.and/or/xor/andNot` re-read the entire other BitSet into a `Vec<i64>` via `bs_read_words` (phases_early.rs:3125 etc.) on every call, and `set range`/`flip range`/`clear range` call `bs_set_bit` per bit (each re-resolving the words array field and calling `bs_ensure_capacity`) — O(range) field lookups instead of O(words) word-level masking. Hot for large ranges.
- P3 — `p52_message_format_apply` collects the whole pattern into `Vec<char>` (phases_early.rs:7561) per format call; fine for small patterns but allocates on every `MessageFormat.format`.
- P4 — `CompletableFuture` error path formats `format!("{:?}", e)` into a String stored in a field (phases_late.rs:497, 522, 544, 565) on every failed async op; debug-formatting the error enum is wasteful and loses the typed exception.
- P5 — `ois_write_bytes` writes one byte at a time via `invoke_virtual(stream,"write","(I)V")` (phases_late.rs:34092) — a full virtual dispatch per byte for serialization output. Batch via `write([B)`.
- P6 — Many natives `read_string` + rebuild a `String` then `create_string` round-trip even when the value is passed through unchanged (e.g. `toPattern`/`toHexDigits` paths), and `p52_read_object_array` allocates a `Vec<Value>` of the whole arg array per format call.

## Tests

Inline `#[cfg(test)]` coverage exists and is reasonably real (driven through the registry with a `MockNativeContext` / `mock_ctx` harness, plus `native-builtins/tests/aes_gcm_kat.rs`). Test modules: phases_early ~42 tests (StringTokenizer countTokens edge cases, ArraysSupport vectorized hash vs a reference impl, time/enum helpers), phases_late ~50 tests (Manifest/Attributes parsing from a byte stream, ForkJoinPool, jar), lib.rs ~120 tests (HexFormat, String.format, annotation overrides, object natives, beans).

Adequacy: these cover a handful of representative natives well (Manifest parser and StringTokenizer are genuinely well-tested, including empty/only-delimiter/multi-delimiter cases), but they touch only a tiny fraction of the thousands of registered methods. The high-risk areas found in this review are essentially untested: no negative-index BitSet test (the B1/V1 bug), no Scanner(InputStream) test, no FFM MemorySegment test, no SubmissionPublisher delivery test, no Future/Executor scheduling or timeout test, no ObjectInputStream non-trivial-type test, no UTF-8-boundary Scanner test. The tests are happy-path and do not probe the spec divergences or the panic/overflow surfaces.

Estimated coverage: ~22%. Basis: a few subsystems (Manifest/Attributes, StringTokenizer, HexFormat formatHex, String.format, ArraysSupport hash) have direct unit tests; the overwhelming majority of registration surface (FFM, serialization, Process, Flow, concurrency Futures, Scanner I/O, BitSet, MessageFormat/DateFormat, Charset, foreign-memory, ForkJoinPool internals) has no direct test in these files. Does NOT plausibly reach 85%.

Most important missing tests:
1. BitSet mutators with negative and very-large indices (assert IndexOutOfBoundsException, not panic/OOM) — covers B1/V1.
2. Scanner(String) UTF-8 multi-byte token boundaries + Scanner(InputStream) data flow — covers B3/B4.
3. Future/CompletableFuture: executor honored, timeout overload, exception propagation as a typed Throwable — covers B5/P4.
4. SubmissionPublisher fan-out to a registered subscriber — covers B6.
5. ObjectInputStream round-trip for int/long/double/UTF and at least one non-string object tag — covers S3.
6. essential_quarkus_locale_convert("all") asserts ROOT; locale tag parsing — covers B2.
7. HexFormat.formatHex bounds (negative from/to, from>to) and correctness vs a reference — partially covered, extend.

## Feature Suggestions

1. Add a registration-time assertion (debug builds) that a native's registered descriptor arity matches a declared expected-arg count, eliminating the latent `args[1]` panic class (V2) wholesale.
2. Provide a `--dump-native-registry` diff against a real JDK method census to flag every `Bridge`-tagged native that returns a constant / no-ops (auto-detect candidate stubs like S1–S13 for re-tagging as SyntheticStub).
3. Implement a real FFM `MemorySegment` backed by the existing off-heap arena store (the net/Unsafe arena handles in MEMORY.md already exist) so FFM apps work instead of reading zeros.
4. Wire `Future`/`CompletableFuture`/`SubmissionPublisher` to the real `SharedVm.virtual_scheduler` carrier pool so async/timeout/subscriber semantics match the JDK.
5. Centralize negative-index validation into a `bit_index_arg(args, idx)` helper that throws IndexOutOfBoundsException, and use it across BitSet (and audit other index-taking natives).
6. Replace per-byte `format!("{:02x}")` and per-byte `invoke_virtual(write)` with shared hex-encode / bulk-write helpers used across HexFormat, serialization, and process I/O.

## Files sampled vs fully read

- `native-builtins/src/lib.rs` — sampled (structure-grepped fully). Deep-read: header/property helpers (1-321), `essential_dur_parse` + `essential_quarkus_locale_convert` (54-321), `register_app_stubs`/gating (876-945, 1677-1685), `obj_arg`/`native_not_implemented`/object natives (10765-11010), print/stream natives (11324-11910), `register_hex_format_real_jdk_natives` (6404-6676), SecureRandom OS entropy + unsafe (31375-31413). Not read: the bulk of the 11k-38k register body and reflection/annotation internals.
- `native-builtins/src/phases_early.rs` — sampled + deep-read: BitSet (2748-3135) in full, Scanner (1986-2105), URL encode/decode (7190-7329), MessageFormat (7483-7625), function registry index (19-50 grep), test module (14759-14858). Not read: calendar/timezone/currency/phaser/forkjoin/scheduled-executor bodies.
- `native-builtins/src/phases_late.rs` — sampled + deep-read: executors/Future/CompletableFuture (385-614), Process model (7180-7310), SubmissionPublisher/Flow (16140-16180), FFM Arena/MemorySegment/ValueLayout (25938-26175), ObjectInputStream (34287-34446), GZIP/asFileAttribute notes (34060-34085), ForkJoinPool common pool (42840-42949), unsafe OS-memory FFI (12210-12271), test module (43297-43385). Not read: the large middle registration bodies (beans, nio, charset decoders, reflection) beyond grep-level.

Sampling strategy: structure-grepped all three files, then deep-read the highest-risk surfaces (untrusted-index parsers, concurrency/process/FFM stubs, unsafe blocks, the explicit stub/no-op markers). Prioritized depth on risk over breadth per the brief.
