# Real JDK bytecode and synthetic compatibility code

CratonVM loads and executes real JDK class files by default. A registered Rust
native may either be a required VM/OS bridge, a semantics-preserving intrinsic,
or a synthetic compatibility implementation.

- **Bridge:** behavior that cannot be expressed by ordinary Java bytecode in
  this VM, such as an OS syscall or VM metadata operation.
- **Intrinsic:** a reviewed fast path whose result must match the Java method.
- **Synthetic stub:** fabricated or approximate behavior that can shadow real
  JDK bytecode. This category is compatibility debt.

The shipping defaults use real JDK paths for annotations, AQS/locks,
ForkJoinPool, sockets, FileWriter, and RandomAccessFile. Legacy synthetic paths
are explicit diagnostic opt-ins (`CRATONVM_SYNTHETIC_*` tokens), never the
silent production default.

## The three configurations, and which one you are running

Two independent settings decide what a run executes. They are orthogonal, and
conflating them is the most common source of confusion about this VM.

**Which class library** — `JdkMode`, selected by `--real-jdk` / `--synthetic-jdk`.

| | class library | availability |
|---|---|---|
| `real-jdk` | the JDK's own `jmods`/`lib/modules` bytecode | the launcher default; needs a JDK on the host |
| `synthetic-jdk` | CratonVM's own Rust re-implementation | requires the `synthetic-jdk` Cargo feature, which is **not** in any default build |

**Which substitutions are permitted** — `CompatibilityMode`, selected by
`--jdk-only`. Default `compatible` allows Bridges, Intrinsics *and* synthetic
stubs. `--jdk-only` makes the real class bytes authoritative: no fabricated
compatibility class, no `SyntheticStub` registered or invoked, and a structured
error instead of a silent substitution. Strictness is a runtime policy, never
inferred from a Cargo feature or an environment variable.

So the shipped default — real class library, permissive policy — is genuinely a
hybrid, and the ratchet census measures exactly how much:

| configuration | total registrations | `SyntheticStub` |
|---|---|---|
| `real-jdk`, `compatible` (the default) | 11,639 | **685** |
| `real-jdk`, `--jdk-only` | 10,954 | **0** (737 registrations refused) |

The strict configuration is therefore already reachable and already stub-free by
construction; what remains is behavioural completeness of the real paths that
replace those 685. Two further numbers from the same gates size the rest of the
work: **1,148** registrations are shadowed (a later `register()` wins over an
earlier one for the same class+method+descriptor), and **51** of those pairs
disagree about their `NativeKind`.

Run the census yourself with `--dump-native-registry`, or reproduce the table
with `cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture`
and `--test duplicate_registration_gate`.

## Change policy

Do not add a `SyntheticStub` registration when a Bridge, Intrinsic, or real
bytecode path can implement the contract. The exact census in
`native-builtins/tests/stub_ratchet.rs` fails on a one-registration increase.
When a stub is removed, lower the baseline in the same change.

Every default-path flip needs:

- a direct default-mode runtime test;
- an opt-out test if the legacy fallback remains;
- GC, thread, and shutdown coverage for concurrent/I/O subsystems; and
- documentation in [CONFIG.md](CONFIG.md).

See the [contributor policy](contributing/no-synthetic-stubs.md) and
[ratchet procedure](contributing/stub-ratchet.md).
