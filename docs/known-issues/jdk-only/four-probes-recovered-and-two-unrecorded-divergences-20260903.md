# Four probes recovered from a sibling worktree, and two divergences nobody has recorded

**Status: MEASURED 2026-09-03** on `azure-host-2` (`azureuser@20.80.105.49`),
binary `/data/l7dod-target/debug/cratonvm` built from this tree, oracle Temurin
25 on the same host. **VERIFIED AGAINST A BINARY 2026-09-03.**

> **Both unrecorded divergences are addressed 2026-09-04 — one closed, one
> narrowed, and the difference is stated rather than blurred.**
>
> **`LinuxAsynchronousChannelProvider` — CLOSED.** §4 recorded
> `asc.provider()` answering `null` where HotSpot answers the platform
> provider, and named the consequence: *"`NullPointerException` for any caller
> that uses the documented `provider().openAsynchronousChannelGroup(…)`
> route."* Measured now, both shipping arms:
>
> ```text
>                                             HotSpot 25                        CratonVM
> AsynchronousSocketChannel.provider()        LinuxAsynchronousChannelProvider  same
> AsynchronousServerSocketChannel.provider()  LinuxAsynchronousChannelProvider  same
> DatagramChannel.provider()                  EPollSelectorProvider             same
> ```
>
> The third row was NOT in this record: `DatagramChannel.provider()` was `null`
> too, and the probe that found the first two never asked. Recorded here because
> a family with one member measured and two unmeasured is how the next one gets
> missed.
>
> **`MinimalFuture` — NARROWED, not closed, and the remaining half is the
> larger half.** §4 recorded that `HttpClient.sendAsync` returns *"a plain
> `CompletableFuture` that is already done"* where HotSpot returns a
> not-yet-complete `MinimalFuture`. Re-measuring found something worse that the
> §4 table could not show, because §4 asked a reachable endpoint: against a
> REFUSED port, `sendAsync` did not return a future at all —
>
> ```text
>                     HotSpot 25                                  CratonVM (before)
> sendAsync (refused)  jdk.internal.net.http.common.MinimalFuture  threw java.io.IOException
> ```
>
> `CompletableFuture<HttpResponse<T>> sendAsync(...)` declares no checked
> exception, so that is a throwable no Java implementation of the method could
> produce and no caller can catch without `catch (Throwable)`. The failure now
> completes the returned future instead, which is where the contract puts it —
> `.get()`/`.join()` raise `ExecutionException`/`CompletionException` as on
> HotSpot.
>
> Getting there needed the right variant: the failure arrives as
> `InternalError(VmError::Runtime(..))`, not as an already-materialised
> `ExceptionThrown`, so the first attempt matched an arm that never fired and
> changed nothing observable. It is built through
> `RuntimeError::as_java_throwable` — the same table the interpreter's own throw
> site uses, so the two cannot drift.
>
> **What §4's original observation still stands on.** The request is performed
> SYNCHRONOUSLY, so:
>
> ```text
>                    HotSpot 25         CratonVM (now)
> sendAsync.class    MinimalFuture      CompletableFuture
> sendAsync.isDone   false              true
> ```
>
> A caller that chains on the future still sees the continuation run on the
> calling thread rather than a client thread, and one that inspects the type
> still sees the wrong class. Making the send genuinely asynchronous is a
> different change and is not attempted here. The two rows above are the
> unclosed part, and they are the part §4 was written about.

**Lane** L7. **Subject** the probes `W7-49` and `W7-66` name, and what running
them says.

---

## 1. The handles were not in the tree

`W7-49` and `W7-66` name four probes between them. **None was in this
worktree.** `probes/` was deleted wholesale by `3b2901531` ("major doc
consistency update before the release"), taking these with it — the same
deletion that had already cost `ShadowDifferentialProbe`, whose own record
points at a `$SCRATCH/probesrc/` path that never survived its session at all.

All four were found in a sibling worktree on the same host:

```text
/data/wt-tomcat-3gc-20260821/probes/SlotIndexRecensusProbe.java     181 lines
/data/wt-tomcat-3gc-20260821/probes/OverAllocationWidthProbe.java   574 lines
/data/wt-tomcat-3gc-20260821/probes/UnderAllocationProbe.java       242 lines
/data/wt-tomcat-3gc-20260821/probes/GuardedSlotMapProbe.java        174 lines
```

**Provenance was checked, not assumed.** A file copied out of another lane's
working tree could be an uncommitted local edit. Each was hashed with
`git hash-object` and the blob confirmed present in this repo's object store
(`git cat-file -e`), so all four are committed content that the deletion removed
from the checkout but not from history.

## 2. What they measure today

```text
                          HotSpot   CratonVM   differing
SlotIndexRecensusProbe      14         14          4
OverAllocationWidthProbe    69         69          5
UnderAllocationProbe        47         47          4  -> 1 after §3
GuardedSlotMapProbe         23         23          0
```

## 3. Three of those lines were the probe, not the VM

`UnderAllocationProbe` prints paths from
`Files.createTempFile("underalloc", …)`, which picks a fresh number every run.
Three of its four "divergences" were the FILENAME — a diff between HotSpot and
CratonVM that would also appear between two runs of either one.

The rendering is now normalised (`underalloc<n>.txt`) and **the comparison is
untouched**: `check` still tests the real values, which come from the same run
and still have to be equal, so a genuine mismatch still FAILs and still prints
both sides. After the fix:

```text
HotSpot self-diff across two runs   0 lines      (it was not stable before)
CratonVM vs HotSpot                 1 line       (was 4)
```

Same defect, same fix, as the ephemeral ports in
`HttpServerWildcardAddressProbe`. A probe whose output is unstable across two
runs of one binary cannot be diffed against another binary at all, and the
reader has to be told which lines to ignore — which is how a real divergence
gets ignored along with them.

## 4. The ten real divergences, and two of them are unrecorded

```text
                                    HotSpot                          CratonVM
cf.class                MinimalFuture                     CompletableFuture
cf.isDone               false                             true
subject.principals      Collections$SynchronizedSet       HashSet
asc.provider            LinuxAsynchronousChannelProvider  null
ssc.provider            EPollSelectorProvider             null
sc.provider             EPollSelectorProvider             null
ssc.keyFor.afterSocket  registered=true                   registered=false
ts.headSet.spliterator  9223372036854775807               3
force(0,8)              true                              UnsatisfiedLinkError
                                                          MappedMemoryUtils.force0
```

Searched across the whole known-issues tree and the retired/internal one:

* **`MappedMemoryUtils.force0` — recorded**, `W7-68-live-under-allocations.md`.
* **`EPollSelectorProvider` — recorded**, in `H5-1`, `W7-9` and
  `WORKER-5-NOTE-1`.
* **`MinimalFuture` — NOT RECORDED ANYWHERE.** HotSpot's
  `HttpClient.sendAsync` returns `jdk.internal.net.http.common.MinimalFuture`,
  a `CompletableFuture` subtype, not already complete. This VM returns a plain
  `CompletableFuture` that is **already done**. A caller that chains on it sees
  the continuation run on the calling thread instead of a client thread, and a
  caller that inspects the type sees the wrong class.
* **`LinuxAsynchronousChannelProvider` — NOT RECORDED ANYWHERE.**
  `AsynchronousSocketChannel.provider()` answers `null` where HotSpot answers
  the platform provider. `NullPointerException` for any caller that uses the
  documented `provider().openAsynchronousChannelGroup(…)` route.

Neither is fixed here. They are named so they stop being invisible: the probes
that show them were out of the tree, so nothing had run them since
`3b2901531`.

## 5. What this does NOT do

It does not discharge `W7-49` or `W7-66`. Both predate these probe versions and
neither predicts any observable above — `MinimalFuture`, `SynchronizedSet`,
`EPollSelectorProvider`, `MappedMemoryUtils` and `headSet` appear in neither
file. The probes grew past the records, so a green or red here is not a verdict
on what those records claim, and adjudicating them needs their own expectation
tables read row by row.

## Reproduce

```bash
source /data/toolchain/env.sh
javac -d /tmp/p probes/SlotIndexRecensusProbe.java probes/OverAllocationWidthProbe.java \
                probes/UnderAllocationProbe.java probes/GuardedSlotMapProbe.java
cd /tmp/p
for p in SlotIndexRecensusProbe OverAllocationWidthProbe UnderAllocationProbe GuardedSlotMapProbe; do
  $JAVA_HOME/bin/java -cp . $p > $p.hs
  cratonvm --java-home $JAVA_HOME -cp . $p | grep -v '^\[cratonvm\]' > $p.cv
  echo "$p $(diff $p.hs $p.cv | grep -c '^<')"
done
# and run each twice on ONE vm first -- a probe that is not self-stable
# cannot be diffed against the other one
```
