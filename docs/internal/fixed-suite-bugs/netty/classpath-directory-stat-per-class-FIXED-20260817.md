# `DefaultThreadFactoryTest.testDescendantThreadGroups` — not ZGC, not CratonVM: a `stat` per classpath directory per class

**Status: FIXED 2026-08-17**, branch `fix/netty-dns-ftl-zgc-20260817`. Retires
`known-issues/netty/defaultthreadfactorytest-zgc-timeout-20260816.md`, whose
central claim ("a real, CratonVM/ZGC-specific slowdown") is disproved below.
The test failure was real and reproducible; every word of its attribution was
wrong, and the thing actually behind it is a class-loading defect that costs
every fork of every suite.

## What the 08-16 page had, and why it pointed the wrong way

It had three isolated ZGC runs failing on the same method and G1/generational
passing, and it read that as ZGC-specific. That inference needs the two arms to
differ by more than the measurement's own noise, and here they do not.

| arm | `testDescendantThreadGroups` | rest of the class |
|---|---|---|
| CratonVM ZGC | 2451 ms, **FAILED** (`@Timeout(2000)`) | 17-33 ms each, all pass |
| CratonVM G1 | 2566 ms, **FAILED** | 18-96 ms each, all pass |

G1 fails too. It passed in the 08-16 runs only because it landed a little
under a 2000 ms line the method was already sitting on; adding one JUnit
listener, or arming `--stack-sample-ms`, puts G1 over it as well. The two
collectors differ by ~100 ms against a 2.4 s cost — noise, not a signal.

Three more measurements finish the demolition:

* **It is the FIRST method that runs**, and the other four in the class cost
  17-96 ms. Nothing about the method is slow; it pays a one-time cost.
* **The cost is netty's first logger touch.** `FastThreadLocalThread.<clinit>`
  → `InternalLoggerFactory.getInstance` → slf4j → logback → joran XML config.
  `DefaultThreadFactory.newThread` alone is 2784 ms of the method's 2451-2566
  ms; the second call is under 1 ms.
* **HotSpot on the same host pays MORE.** Interleaved, same classpath, same
  probe: HotSpot 3232 ms, CratonVM 2195 ms. HotSpot never *reports* it because
  JEP 486 makes `System.setSecurityManager` throw, so this method
  `assumeFalse`-skips there — CratonVM deliberately did not adopt JEP 486, so
  it is the only VM that runs the method at all.

So "CratonVM's failure … reads as a real, CratonVM/ZGC-specific slowdown" is
wrong twice over. What the page correctly established is that the method
genuinely fails and does so repeatably.

## The real defect

Netty's suite classpath is **308 entries: 197 JARs and 111 directories** (one
`target/classes` and one `target/test-classes` per reactor module). The
first-touch loads 1102 classes (`CRATONVM_DBG=define-census`, no duplicates).

`ClassPath::find_class` scanned every entry in order. A `JarFile` entry carries
`entry_index` precisely so a miss costs a hash probe — its own comment says so.
`Directory` had no equivalent: it did a real `dir.join(rel).exists()`. A class
lives in at most one entry, so essentially the whole cost is misses:

**111 directories × 1102 classes = 122k `stat` syscalls.**

At ~15-20 µs each on Windows that is the entire 2 s. The A/B is unambiguous:

| classpath | first `newThread` |
|---|---|
| all 308 entries | 2195 ms |
| 197 JARs + the 4 entries actually needed | 143 ms |
| 110 directories + the 4 needed | 1332 ms |
| the 4 actually needed | 117 ms |

Linux hides it completely — the same probe is 483 ms there, **faster than
HotSpot's 644 ms** — because a `stat` on a warm dentry cache is ~1 µs. That is
why no Linux run ever flagged it, and why the 08-16 page, working from Windows
runs alone, had nothing to compare against.

This is the same shape as the `JarFile`-accessor defect in
`a_stat_per_accessor_call_hid_behind_an_o1_cache`, one layer out: a live
filesystem probe on a path everything assumed was a lookup.

## The fix

A per-directory index (`ClassPath::dir_index`) of the relative paths under each
`Directory` entry, built lazily on first use, so a miss is a hash probe.

Correctness is preserved by making the search **two passes**, not by trusting
the index:

* **Pass 1** consults the index and never stats a directory that cannot hold
  the class.
* **Pass 2** runs only when pass 1 found nothing anywhere, and is the
  historical stat-per-directory scan verbatim. A class written into a
  classpath directory *after* it was indexed is invisible to pass 1 and still
  found by pass 2, which then invalidates that directory's index so the next
  lookup rebuilds rather than falling through forever.

Anything the old single pass could find, this finds. The index only removes
syscalls from the path that succeeds.

**Three functions needed it, not one.** Indexing `find_class` alone took the
probe 2195 ms → 1299 ms and left 1332 ms of directory cost behind:
`find_class_source_path` (the origin census) and `find_class_code_source_info`
(`defineClass`'s `CodeSource`) each ran their own copy of the same sweep, once
per class define. All three are indexed.

## Result

`DefaultThreadFactoryTest`, isolated, one class per process, Windows host:

| collector | before | after |
|---|---|---|
| ZGC | FAIL 4/5 — `testDescendantThreadGroups` timed out, 3/3 runs | **PASS 5/5**, 1748 ms |
| G1 | PASS 5/5 (and FAIL under any added load) | **PASS 5/5**, 2134 ms |
| generational | PASS 5/5 | **PASS 5/5**, 2239 ms |

5/5 is the baseline `misc-non-tls-residuals-CLOSED-20260813.md` established for
this class, and the whole class now costs less than the one method used to.

The first-touch probe itself, same host, same 308-entry classpath:

| | before | after | HotSpot 25 |
|---|---|---|---|
| netty's first `DefaultThreadFactory.newThread` | 2195 ms | **1061-1083 ms** | 958 ms |

## What is left, deliberately

**Parity, not victory.** The remaining ~900 ms of directory cost is a *miss*
scan, and HotSpot pays it too: `URLClassPath.FileLoader.getResource` does
`new File(dir, name).exists()` per entry per lookup, with no index, which is
why HotSpot's 958 ms sits right next to CratonVM's 1061 ms. Pass 2 is where it
lands here — a class that is not on the application classpath at all (every
JDK class) misses pass 1 everywhere and falls through to the historical scan.

That could be avoided with a negative memo, and it is deliberately not: the
memo would have to answer "this class is absent" for a class that might be
written into a classpath directory later, which is the one guarantee pass 2
exists to keep. Going below HotSpot on a miss is a different piece of work
from fixing a defect that made us 2.3x worse than it, and only the second one
was in scope here.

## Lessons

**A one-arm difference is not an attribution.** Three ZGC failures and three G1
passes look like a collector finding until you notice the two arms are 100 ms
apart on a 2400 ms cost and the pass/fail line runs between them. Re-check the
"good" arm under the same perturbation before naming the difference — arming
the stack sampler was enough to flip G1.

**Run the other platform before writing "X-only".** Linux is 4.5x faster here
and beats HotSpot; Windows is 2 s and loses to nothing except its own syscall
cost. Either platform alone gives a confident and wrong story. (Same rule as
`run_the_older_binary_on_the_same_platform`, different axis.)

**A test that HotSpot skips has no control arm.** JEP 486 means HotSpot never
executes this method, so "HotSpot passes" said nothing about the workload. The
usable control was not the test but the *cost* — the same class-loading probe
run on both VMs, where HotSpot turned out to be slower.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.util.concurrent.DefaultThreadFactoryTest\n' > /tmp/tf.txt
./run-netty-suite.sh --list /tmp/tf.txt --gc zgc --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/tf.txt --gc g1  --shards 1 --out runs/repro
```

`probes/FirstTouch.java` isolates the cost without JUnit: it times netty's
`DefaultThreadFactory.newThread` first touch, and takes the classpath as an
argument so the 308-vs-4 A/B above is one command each. `CRATONVM_DBG=define-census`
gives the class count.
