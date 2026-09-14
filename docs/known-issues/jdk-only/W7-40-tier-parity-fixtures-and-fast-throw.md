# A tier-parity fixture that compares exception *messages* goes red on HotSpot

**Status: METHOD NOTE, not a defect.** No CratonVM change. Filed because it cost
a wave-lane a false red and will cost the next one the same way — every fixture
in this directory that compares a compiled answer against an interpreted one is
exposed to it.

Filed 2026-08-12, from building `regression-suite/src/RArrayStoreTiers.java` for
`W7-38-jit-aastore-never-called-its-own-check.md`.

---

## 1. What happened

The natural way to write a two-tier fixture is to capture one string per
iteration and require the first and last to agree:

```java
String observe(int id) {
    try { run(id); return "no-throw"; }
    catch (ArrayStoreException e) { return "ArrayStoreException:" + e.getMessage(); }
    ...
}
```

Run on **HotSpot**, the oracle, that fixture fails:

```
s03 Runnable[] as Object[] <- String   cold=[ArrayStoreException:java.lang.String] hot=[ArrayStoreException:null]  MOVED@1375
DIVERGENCE s03 ... HOT: want=[ArrayStoreException:java.lang.String] got=[ArrayStoreException:null]
DIVERGENCE s03 ... TIER-SPLIT at i=1375: cold=[...java.lang.String] became=[...null] final=[...null]
Exception in thread "main" java.lang.AssertionError: 2 divergence(s)
```

The message disappears partway through the run. Nothing about the store changed.

## 2. Cause

`-XX:+OmitStackTraceInFastThrow`, **on by default**. Once HotSpot has thrown an
*implicit* exception often enough from a **compiled** site, it stops
constructing one and substitutes a preallocated instance with no message and no
stack trace. It applies to the exceptions the VM raises itself —
`ArrayStoreException`, `NullPointerException`, `ArrayIndexOutOfBoundsException`,
`ClassCastException`, `ArithmeticException` — and not to `throw new ...` in user
code.

Confirmed by flipping exactly that switch and changing nothing else:

| run | result |
|---|---|
| `java RArrayStoreTiers` (message compared across tiers) | `AssertionError: 2 divergence(s)`, `MOVED@1375` |
| `java -XX:-OmitStackTraceInFastThrow RArrayStoreTiers` | `PASS (48 checks)` |
| `java -Xint RArrayStoreTiers` | `PASS (48 checks)` |

## 3. Why it is a good trap

Everything about it argues for "real VM bug" on first contact:

* It is **per-site**. Four other shapes in the same fixture threw
  `ArrayStoreException` 3000 times each and kept their messages; only `s03` lost
  its own. So it does not look like a systematic rule, it looks like corruption.
* It is **timing-dependent** — `MOVED@1375` moves between runs — so it reads as
  a race.
* It appears **only in the compiled tier**, which is precisely the hypothesis a
  tier-parity fixture is built to test. The fixture returns exactly the signal it
  was written to detect, from the wrong subject.
* The fixture was, at that moment, a *new* fixture for a *known* defect, so the
  prior probability that it had found something was high.

The general form: **when a differential fixture goes red on the oracle, the
fixture is wrong until proven otherwise** — and "red on the oracle in the same
shape as the defect you are hunting" is the most expensive version of that,
because it confirms the hypothesis instead of contradicting it.

## 4. The rule this yields

Separate the observable by whether it is actually tier-invariant.

* **Exception KIND is tier-invariant.** Whether a store is refused is the
  property a store check has. Assert this across tiers.
* **Exception MESSAGE is not.** The oracle is entitled to drop it in compiled
  code. Assert messages only on the **cold** (interpreted) answer, where both
  VMs are deterministic — a separate pass, before any warm-up loop has run.

`RArrayStoreTiers` is structured this way: pass 1 executes each site once and
checks kind-plus-message; pass 2 loops each site and checks kind only. It is
green on HotSpot with the JIT on **and** under `-Xint` (63 checks both), and red
on an emulated defect.

Do **not** fix this by adding `-XX:-OmitStackTraceInFastThrow` to the oracle
invocation. It makes the fixture pass while leaving it asserting a property the
subject is not required to have, and the flag will not be there the day someone
runs the fixture by hand.

## 5. Consequence for existing records

`regression-suite/src/RExceptions.java` asserts `ArrayStoreException` **message**
parity across warm-up at `i=500`, over 1200 iterations
(`"ArrayStoreException text moved during warm-up at i="`). On CratonVM today that
assertion is red for a real reason — W7-38 — and its `hot=[no-throw]` reading is
not a fast-throw artefact, because `no-throw` is the absence of an exception, not
a message-less one.

But once W7-38 is fixed, that assertion becomes exposed: it will be comparing
message text across a tier boundary on a VM that may legitimately adopt the same
optimisation, and its 1200 iterations are already within the range where HotSpot
elides (`MOVED@1375` above is the same order). **Do not read a future red there
as a W7-38 regression without checking whether the message went to `null` rather
than to `no-throw`.** The two are different findings and the assertion text does
not distinguish them.
