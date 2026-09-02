# The ratchet that saw five of sixty-one, and the probe that could not be scored

**Status: MEASURED and FIXED 2026-09-02** on `azure-host-2`
(`azureuser@20.80.105.49`), tree `94a887093`, binary
`/data/l7dod-target/debug/cratonvm` built 2026-09-02 00:43. Everything below was
run; nothing is predicted.

**VERIFIED AGAINST A BINARY 2026-09-02.** Section 4's arms were run on the
binary named above, both clean. This record carries the discharge phrase for a
second reason worth knowing: it NAMES the test file, and the file name contains
the very substring the detector matches. A record that discusses the ratchet
trips the ratchet. That is the cost of matching substrings instead of parsing,
and it is the right trade -- the alternative missed 56 pages.

**Lane** L7 (`--jdk-only` definition-of-done), continuing
[`the-five-owed-records-run-at-last-20260902.md`](the-five-owed-records-run-at-last-20260902.md).

**Files** `types/tests/unverified_records.rs`,
`probes/HttpServerWildcardAddressProbe.java`,
[`W7-24-httpserverloop-and-strict-fallbacks.md`](W7-24-httpserverloop-and-strict-fallbacks.md).

---

## 1. The number

`types/tests/unverified_records.rs` exists to count records that claim a fix
nobody has ever run. It was written on 2026-09-01, it was green, and it was
counting **5**.

The real population is **61**.

Fifty-six pages said, in their own status block, that no VM had ever executed
the code they describe, and the gate was green over every one of them.

## 2. How a green gate saw almost nothing

The detector held a list of six exact phrases. It was built by reading the four
records the test was born from, which is precisely how you build an instrument
that can only find what you already found.

The campaign says the same thing many other ways:

```text
FIXED-UNVERIFIED                         FIXED-UNVERIFIED-BY-CARGO
FIXED-UNVERIFIED-ON-CRATONVM             FIXED-UNVERIFIED against CratonVM
CODE LANDED, BEHAVIOUR UNVERIFIED        FIXED IN SOURCE, NOT VERIFIED BY AN ARM
"no binary exists that contains the code below"
"the fix is written and formatted but has not been built"
"no binary carrying these changes has been built or run"
```

None of the six phrases matches any of those.

**The guard that was supposed to catch this was working perfectly.** The test
carries `MIN_PAGES_SCANNED = 200` — a floor on the walk, added because a
mistyped path would otherwise pass as a confident zero. The walk saw **526**
pages. It was never the walk. **A floor on the denominator says nothing about
the numerator**, and this repo's other census gates should be read with that in
mind: several of them bound their input and then match with a literal list.

## 3. What it is now

Markers are matched **case-insensitively as substrings**, and the list is short
and generic (`unverified`, `not rebuilt`, `no binary`, `built or run`, ...). A
marker that names a lane, a date or a file is one that will miss the next
record.

The discharge marker stays an **exact** phrase, deliberately asymmetric with the
above. A draft loosened it to also accept `verification note` and `RE-VERIFIED`;
it promptly declared `W7-24` and `W7-57` discharged, both of which are in the
allow-list precisely because they are not. Detection should be generous;
absolution should not be.

All 61 are enumerated in `ALLOWED`, each with the status line that put it there.
That keeps the gate green — this is a worklist, not a wall — while making the
debt something a person can pick up.

**The list cannot rot.** A new check requires every entry to still name a page
that exists and still trips the detector. Pay a debt and the entry MUST be
deleted, so the list only shrinks; a stale allowance is how 61 real debts decay
into a list nobody trusts and then into one nobody reads.

**Both halves were mutation-checked**, because a green from a test you just
rewrote is worth nothing:

```text
remove H5-1's entry          -> red, naming H5-1 ("says: unverified")
allow the verified W7-63     -> red, "the debt was paid and the entry outlived it"
```

## 4. The first debt paid: `W7-24`

`W7-24` said `HttpServer.start()` could not run under `--jdk-only` —
`NoClassDefFoundError: CratonVM$HttpServerLoop` — and that the fix was
**applied but never rebuilt**, on 2026-08-11. Twenty-two days.

```text
--jdk-only   rc=0, all seven rows printed, no NoClassDefFoundError
--real-jdk   rc=0, byte-identical to --jdk-only
```

Fixed, and right the whole time. That is now the fifth record in a row whose
unrun fix turned out to be correct: the work is not the problem, the run is.

### Getting there took three repairs, each hiding the next

**The probe could not be scored at all.** Every bind asks for port 0, so the
kernel picks a new number each run: a raw diff reported four differing rows on
two runs of the SAME binary. It now erases the trailing `:<port>` and keeps "a
port was bound" as its own boolean — the fact of binding is signal, and a probe
that hid a failure to bind behind its own normalisation would be worse than the
unscoreable one. Both VMs are now self-stable across two runs.

The rewrite touches only the trailing port, which matters here: HotSpot answers
`/[0:0:0:0:0:0:0:0]:PORT`, whose ADDRESS is full of colon-digit pairs a careless
rewrite would eat — destroying the exact difference the probe exists to see.

**The probe's header described a fix it had itself caused.** It presented the
`0.0.0.0` -> `127.0.0.1` rewrite in `advertised_listener_host` as current. That
rewrite was REMOVED on 2026-08-10 *using this probe*, and the measurement is
quoted in `native-io/src/socket_channel.rs`. A probe that misdescribes the code
it measures sends its next reader hunting a fix that already landed.

**The arm was wrong, in the direction opposite to last time.** The 2026-09-02
run recorded that these probes need `--synthetic-jdk`. `W7-24`'s own reproduce
line is `--jdk-only`. Read the record for its arm; do not inherit the
neighbour's.

## 5. What still differs, and one finding nobody held

Four of seven rows differ from HotSpot, all one thing: HotSpot binds the
wildcard as a dual-stack IPv6 socket and reports `[0:0:0:0:0:0:0:0]`, CratonVM
reports `0.0.0.0`. `isAnyLocalAddress()` is `true` on both, so a caller asking
the behavioural question gets the right answer. `socket_channel.rs` names this
as knowingly left open.

But the layers DISAGREE, which that comment does not predict. On the real-JDK
arms the raw `ServerSocketChannel` row matches HotSpot exactly; only the
`HttpServer` above it answers v4. So the residual is not merely `ssc_bind`
creating a v4 listener.

The registry dump says why:

```text
sun/net/httpserver/HttpServerImpl.getAddress    invocations = 2
com/sun/net/httpserver/HttpServer.getAddress    invocations = 0
com/sun/net/httpserver/HttpServer.create        invocations = 2   <- static, the control
```

The probe called `getAddress()` exactly twice. `HttpServer` is **abstract**,
every concrete subclass must override, and the dispatch door asks the DECLARING
class — so **nine instance-method registrations on
`com/sun/net/httpserver/HttpServer` are unreachable**, with the `HttpServerImpl`
twins carrying the traffic. The static `create` on the same class is the control
that makes this readable rather than a guess: same dump, same class, counter
demonstrably works, so the zeros are real.

This is the family
[`H5-1`](H5-1-the-abstract-registrations-are-fabricated-receivers-20260820.md)
is about — and `H5-1` is itself one of the 56 that were invisible until today.
Recorded here, not fixed here: deleting a dead registration is a change to
`net_phase_e.rs`, which belongs to whoever owns that file.


### WITHDRAWN 2026-09-02 — the nine registrations are NOT unreachable

The paragraph above is **wrong**, and the error is worth keeping visible because
the reasoning looked sound.

`HttpServer.create()` — the NO-ARG factory — mints a receiver whose runtime
class IS `com/sun/net/httpserver/HttpServer`:

```text
com/sun/net/httpserver/HttpServer   11 rows, 11 invocations   <- the no-arg door
sun/net/httpserver/HttpServerImpl   11 rows,  1 invocation    <- the two-arg door
```

`HttpServerWildcardAddressProbe` only ever calls `create(InetSocketAddress, int)`,
which mints `HS_IMPL_CLASS`. Every zero I read was a statement about the door my
probe took, and I published it as a statement about the class.

**The general rule was right; the per-row check is what I skipped.** A
registration on an abstract class is unreachable *unless this VM mints a carrier
under that exact name* — and `re10_create_unbound_server` does exactly that, with
a comment at the mint site saying so. One `grep` for the class name as a literal
would have found it. Reading `HttpServerImpl`'s rows as independently-authored
twins was the same mistake twice: they are an `alias_class` SNAPSHOT of the
public class's rows, not a second author.

**What the investigation did find is worse than dead rows**, and has its own
page: minting the public name handed the application an instance of an ABSTRACT
class, and `HttpServer.createContext(...)` returned a context whose every
accessor threw `AbstractMethodError` on both shipping arms. Four defects, all
now fixed and verified. See
[`the-httpserver-family-four-defects-20260902.md`](the-httpserver-family-four-defects-20260902.md).

## Reproduce

```bash
source /data/toolchain/env.sh
cargo test -p cratonvm-types --test unverified_records

# the probe, both arms, twice each -- the second run is the point
javac -d /tmp/p probes/HttpServerWildcardAddressProbe.java
cd /tmp/p
$JAVA_HOME/bin/java -cp . HttpServerWildcardAddressProbe > hs1.txt
cratonvm --java-home $JAVA_HOME --jdk-only -cp . HttpServerWildcardAddressProbe > cv1.txt
diff hs1.txt cv1.txt      # 4 rows, all the v4/dual-stack wildcard

# which door answers getAddress()
cratonvm --java-home $JAVA_HOME --real-jdk --dump-native-registry reg.json \
  -cp . HttpServerWildcardAddressProbe
```
