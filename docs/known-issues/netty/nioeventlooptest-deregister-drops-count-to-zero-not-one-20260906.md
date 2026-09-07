# `NioEventLoopTest.testChannelsRegistered` — deregistering one of two channels dropped the count to 0, not 1

**FIXED 2026-09-07 — this page moved.** Root cause and fix now live in the
retired `nioeventlooptest-deregister-count-selector-keys-cancelled-filter`
write-up under `fixed-suite-bugs/netty/`: `Selector.keys()` was filtering
out cancelled-but-unpruned keys,
which double-counted against Netty's own
`numRegistered() = keys().size() - cancelledKeys` bookkeeping. Fixed in
`native-io/src/nio_selector.rs`'s `selector_keys()`; verified with two
deterministic runs of the real class (`found=13 ok=13 failed=0`).
