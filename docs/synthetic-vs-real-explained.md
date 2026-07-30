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
