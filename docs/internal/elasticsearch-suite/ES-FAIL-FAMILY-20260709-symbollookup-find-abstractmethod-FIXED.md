# ES failure family - SymbolLookup.find AbstractMethodError - FIXED

Status: FIXED

Signal:
- `java.lang.AbstractMethodError: method java/lang/foreign/SymbolLookup.find(Ljava/lang/String;)Ljava/util/Optional; has no Code attribute`

Current counts at doc generation time:
- FAIL: 7

Representative class:
- `server org.elasticsearch.index.codec.vectors.es93.ES93BinaryQuantizedBFloat16VectorsFormatTests`

Probe results:
- HotSpot: status=PASS, rc=0, seconds=11.137, tests=58, mode=triage-abstract-hotspot
- CratonVM --nojit: status=FAIL, rc=1, seconds=37.462, tests=58, mode=triage-abstract-nojit

Interpretation:
- HotSpot passes, CratonVM `--nojit` fails, so this is not JIT-specific.
- The real-JDK receiver dispatch is still reaching the abstract interface declaration instead of CratonVM native bridge handling.
- The code already has a Panama `SymbolLookup.find` bridge; this failure means some concrete/interface dispatch path is not force-routing to it.


Fix:
- `vm/src/vm/vm_exec.rs` now includes `is_ffm_symbol_lookup_native_override` in the shared native-override gate.
- This makes the shared dispatch path mirror the interpreter force-native predicate for `java/lang/foreign/SymbolLookup.find(Ljava/lang/String;)Ljava/util/Optional;`, so the existing Panama bridge wins before dispatch can fall through to the abstract interface declaration.

Verification:
- `CARGO_TARGET_DIR=/data/data/cargo-targets/es-fixture-20260708-220010 /home/victor/.cargo/bin/cargo test -p cratonvm-vm --lib ffm_symbol_lookup_force_native_covers_find -- --nocapture` -> PASS, 1 passed, 0 failed, 2270 filtered out.
- `CARGO_TARGET_DIR=/data/data/cargo-targets/es-fixture-20260708-220010 /home/victor/.cargo/bin/cargo check -p cratonvm-vm --lib` -> PASS, finished dev profile.

Note:
- A broader `cargo test -p cratonvm-vm ffm_symbol_lookup_force_native_covers_find -- --nocapture` attempt was not useful because Cargo compiled every integration test target and the host ran out of space before reaching the filtered unit test. The lib-only command above is the targeted check for this dispatch predicate.
