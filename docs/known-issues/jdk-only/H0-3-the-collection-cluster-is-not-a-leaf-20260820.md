# H0-3 — `ConcurrentHashMap` is not a collection problem, it is the floor the VM stands on

**Status: OPEN — MEASURED.** One run of the full 104-vector strict arm with one
prefix armed. No source change. The binary is
`C:/craton/target-jdkonly-h2/release/cratonvm.exe` at `d97cfb28b`, whose
unarmed baseline in the same session is **104 / 104**.

Lane H0 (orchestrator), 2026-08-20.

---

## 1. The command, and why this one

`H4-1` §7b proposed it as a **free pre-flight**: one command, no build, that
would "either clear or kill the CHM cluster". `H4-1` reached its conclusions
from source reading alone and named `java/util/concurrent/ConcurrentHashMap` as
*the one collection family closed under minting* — i.e. the most likely to
retire cleanly.

```bash
TIMEOUT=420 JDK="C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot" \
  CV="C:/craton/target-jdkonly-h2/release/cratonvm.exe" \
  CRATONVM_ARGS="--jdk-only" \
  CRATONVM_ENFORCE_NATIVE_SHADOW="java/util/concurrent/ConcurrentHashMap" \
  bash regression-suite/run.sh
```

**Run over the 104-vector ARM, not the 36-vector screen.** `G90-1` §5 is the
reason: the screen passed two prefixes the arm rejected, because the screen
corpus asked those subsystems nothing. A pre-flight that repeats that mistake
is worse than none.

## 2. The result

```text
REGRESSION SUITE: 93 passed, 11 failed
  ( RCrypto RChmKeySetView RMapResizeGc RMapGcStress RJdkModule RJdkSecurity
    RJdkX509Intercept RJdkLogging RJdkProxyIface RJdkEnumerations
    RServiceLoaderDoubleSource )
```

**104 → 93 on one prefix.** `H4-1`'s mechanism is confirmed by a run rather
than only by source reading: enforcing §1.4 for CHM hands the container to real
bytecode, which reads a real, empty `table`, because the contents live in a
CratonVM side structure that the 168 direct Rust calls keep writing.

## 3. The finding is *which* eleven, not that there are eleven

Three of the eleven are collection vectors: `RChmKeySetView`, `RMapResizeGc`,
`RMapGcStress`. **The other eight are not:**

| vector | subsystem |
|---|---|
| `RCrypto`, `RJdkSecurity`, `RJdkX509Intercept` | JCA — providers, algorithm tables, certificate interception |
| `RJdkLogging` | `java.util.logging` |
| `RJdkModule` | JPMS |
| `RJdkProxyIface` | dynamic proxies |
| `RJdkEnumerations` | enumeration/iteration surface |
| `RServiceLoaderDoubleSource` | `ServiceLoader` |

**`ConcurrentHashMap` is load-bearing under the VM's own subsystems.** Crypto
provider chains, the logger registry, the module graph, the proxy cache and
service loading are all built on a map whose contents the VM owns. That is why
this cluster has resisted every retag: it is not a leaf of the object model,
it is the floor the rest of it stands on.

Two records are corrected by this:

* **`H4-1` §7b** called CHM *"the one family closed under minting"* and the
  cheapest thing to clear. It is neither — it is the most expensive, and the
  eight non-collection failures are the reason. `H4-1`'s *mechanism* is right
  and its *ranking* is wrong; the ranking was the half derived without a run.
* **`G88-1` §5**'s "retag the whole cluster together and three of five vectors
  go green" measured a cluster boundary drawn around `java.util`. The boundary
  is wrong. The eight vectors above construct no map through a `java.util`
  collection vector, which is `HANDOFF-20260819.md` §3's shape again: **a gate
  whose stated population is narrower than the real one reports a pass.**

## 4. What this means for the P0 table

The *Wholesale `Bridge` over-tagging* row's remedy is "retag, and it is safe
incrementally in a way under-tagging never was". **Measured: it is not safe
incrementally for this family at any granularity**, because the increment that
would make it safe — moving the state — is not a retag.

`HANDOFF-20260820.md` §0 says the P0 table cannot reach CLOSED by retagging.
This is the first *run* behind that claim rather than a source argument, and it
raises the price: the ordering constraint is not "collections first, then the
subsystems that use them". It is the reverse of what the row implies —
**nothing above CHM can be migrated to real bytecode until CHM's state is
real**, and CHM sits under crypto, logging, modules, proxies and service
loading.

## 5. What this does NOT establish

* **It does not say the other collection families are equally bad.** Only CHM
  was armed. `java/util/HashMap`, `LinkedHashMap`, `TreeMap`, `HashSet` are
  each one command away from having the same question answered, and the
  per-family blast radius is exactly the map `H4-1` O2 needs to sequence the
  migration. **Do those five runs before planning the migration order** — they
  cost 20 minutes each and no build.
* **It does not attribute the eight non-collection failures to a single
  mechanism.** They are consistent with "these subsystems store state in a CHM
  the VM owns", and nothing here rules out a second cause. Each one names its
  own assertion in the log; nobody has read them.
* **It says nothing about compatible mode.** The dial only makes §1.4 enforced;
  `--real-jdk` is unaffected.

## 6. NOMINATIONS

* **N1 — run the other five families.** `java/util/HashMap`,
  `java/util/LinkedHashMap`, `java/util/TreeMap`, `java/util/HashSet`,
  `java/util/Hashtable`, one prefix each over the 104-vector arm. The output is
  a blast-radius table, which is the sequencing input the real-construction
  migration has never had.
* **N2 — read the eight non-collection failures.** If they all reduce to
  "reads an empty table", that is one defect with eight faces and the migration
  has a single entry point. If they do not, the count of independent problems
  is larger than anyone has assumed.
* **N3 — this pre-flight belongs in the arms as a scheduled matrix**, not as a
  thing someone remembers to run. It is one env var over an existing run, and
  it is the only instrument that prices a retirement before it is attempted.
