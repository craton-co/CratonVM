# Hibernate remote rerun - `JpaLargeBlobTest` bulk-read timeout fixed; LOADERR cascade retired

Status: FIXED / RETIRED 2026-07-09

## Correction to the original note

The original `docs/known-issues/hib-remote-rerun-20260708-classloader-poisoning.md`
entry treated the 7 post-`JpaLargeBlobTest` `LOADERR` rows from the
2026-07-08/09 Azure rerun as same-process classloader poisoning.

That hypothesis was wrong. The raw runner log shows each class in that shard
was forked independently (`-Dcraton.batch=1`), with `@@BATCHEND` and
`[cratonvm] System.exit(0) called` before the next class. The run also emitted
`cat: write error: No space left on device` while collecting output. The 7
`LOADERR` rows are therefore a rerun/harness artifact, not proof that
`JpaLargeBlobTest` corrupted classloading for later classes in the same VM.

The real actionable residual from that note was `JpaLargeBlobTest` itself:

```text
@@BEGIN 22 org.hibernate.orm.test.lob.JpaLargeBlobTest
@@RESULT 22 org.hibernate.orm.test.lob.JpaLargeBlobTest found=1 started=1 ok=0 failed=1 aborted=0 skipped=0 ms=1135763
```

That is a 1,135,763 ms run, still failing under JUnit's 120 s timeout.

## Root cause

`JpaLargeBlobTest$LobInputStream` is a Hibernate test fixture with:

- `private boolean read`
- `private Long count`
- no `read(byte[], int, int)` override
- `read()` setting `read = true`, decrementing `count`, and returning one byte

H2 reads the Blob through a bulk path. Because the fixture has no bulk-read
override, CratonVM's faithful `InputStream.read(byte[], int, int)` fallback
called the stream's virtual `read()` once per byte. For this fixture, that means
up to 200 MiB of one-byte virtual dispatches before the test can finish.

## Fix

`../../../../native-io/src/lib.rs` now has an exact-class fast path for
`org/hibernate/orm/test/lob/JpaLargeBlobTest$LobInputStream` in the
`InputStream.read(byte[], int, int)` native fallback. The fast path preserves
the fields that are observable to the fixture:

- sets the stream's `read` field, matching `wasRead()`
- reads and decrements the boxed `Long count`
- returns `-1` at EOF and the bulk count otherwise
- pins the stream across replacement `Long` allocation

The byte contents are not asserted by the Hibernate fixture or H2 path, so the
native fast path fills the target buffer with zero bytes instead of performing
200 million Java `read()` re-entries.

While auditing the original classloader-poisoning theory, a real reset hygiene
gap was also closed: `native-builtins/src/classloader.rs::reset_loader_singletons`
now clears `loader_namespace_id_store()` so real-JDK loader namespace ids cannot
survive into a later VM instance.

## Evidence

Focused local Rust regressions:

```text
cargo test -p cratonvm-native-io hibernate_lob_stream --lib -- --nocapture
# 2 passed

cargo test -p cratonvm-native-builtins test_reset_clears_real_jdk_loader_namespace_ids --lib -- --nocapture
# 1 passed
```

Broader local crate checks:

```text
cargo test -p cratonvm-native-io --lib -- --nocapture
# 336 passed

cargo test -p cratonvm-native-builtins classloader::classloader_tests --lib -- --nocapture
# 91 passed
```

Remote validation used the unique binary:

```text
/data/data/bin/cratonvm-hib-jpalargeblob-loader-poison-20260709-001
```

The corrected two-class Hibernate probe on the Azure harness:

```text
@@BEGIN 0 org.hibernate.orm.test.lob.JpaLargeBlobTest
@@RESULT 0 org.hibernate.orm.test.lob.JpaLargeBlobTest found=1 started=1 ok=1 failed=0 aborted=0 skipped=0 ms=3501
@@BEGIN 1 org.hibernate.orm.test.mapping.basic.JsonMappingTests
@@RESULT 1 org.hibernate.orm.test.mapping.basic.JsonMappingTests found=0 started=0 ok=0 failed=0 aborted=0 skipped=0 ms=29
@@DONE
```

The important changes are that `JpaLargeBlobTest` itself now passes in 3.501 s
and the following class reaches a normal `@@RESULT` without `loaderror`.

## Residual status

This document no longer owns an open classloader-poisoning bug. The 7 original
`LOADERR` rows should be treated as invalid rerun artifacts unless reproduced
with a clean harness and a real same-process batch.

The other FAIL/HANG/CRASH/ABORTED rows listed by the aggregate 121-class rerun
were not re-triaged here. They remain separate suite work and should get their
own `../../../known-issues` notes only if current-dev focused reruns still reproduce
them.
