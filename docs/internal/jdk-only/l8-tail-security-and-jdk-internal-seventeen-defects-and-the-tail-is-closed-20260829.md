# L8 tail — `java.security` and `jdk.internal`: 17 defects, and the tail is closed

**Status: CLOSED.** 2026-08-29, branch `claude/l8-tail-20260829`, worktree
`/data/cvm-l2s-20260828`. Oracle: HotSpot `jdk-25.0.4+7`, the same image
CratonVM ran against.

Batches six and seven — the last two. Two probes, **1455 rows, 0 differing lines
in both `--jdk-only` and compatible mode**, from 47.

| probe | rows | before | after |
| --- | --- | ---: | ---: |
| `apps/probes/SecuritySurfaceSweep.java` | 1335 | 20 | **0** |
| `apps/probes/JdkInternalSweep.java` | 120 | 27 | **0** |

---

## 1. `java.security` — 1313 of 1333 rows were already right

A digest is the friendliest thing in this campaign to compare: the output is a
pure function of the input bytes, fixed by a published standard. So the sweep is
**algorithms × inputs × feeding patterns** — ten algorithms, nineteen lengths
chosen to straddle every padding boundary the 512- and 1024-bit blocks use
(55/56/63/64, 111/112/127/128), and six ways of feeding the same bytes in:
`digest(byte[])`, `update(byte[])`, byte-at-a-time, split in three,
heap `ByteBuffer`, direct `ByteBuffer`.

**Every one of those rows was already correct**, along with the whole lifecycle
(reset, implicit reset after `digest()`, `clone` agreeing and being independent,
`getDigestLength` matching the output) and all of `isEqual`. That is the useful
result and it is worth stating first: the digest engine is right, and all twenty
differences were on paths a caller only reaches by getting something wrong.

| # | what | rows |
| --- | --- | ---: |
| S1 | `digest(buf, off, len)` with too small a `len` gave the wrong message | 10 |
| S2 | `getInstance(null)` was `IllegalArgumentException`, not NPE | 1 |
| S3 | `getInstance("")` was `IllegalArgumentException`, not `NoSuchAlgorithmException` | 1 |
| S4 | `update((byte[]) null)` did nothing | 1 |
| S5 | `update(buf, 0, tooLong)` raised the wrong exception CLASS | 1 |
| S6 | `update(buf, -1, 2)` raised the wrong message | 1 |
| S7 | `update((ByteBuffer) null)` did nothing | 1 |
| S8 | **`digest((byte[]) null)` returned null** | 1 |
| S9 | **`doPrivileged` did not wrap a checked exception** | 1 |
| S10 | `CodeSource.getCertificates()` fabricated an empty array where the JDK returns null | 1 |
| S11 | **an empty `ProtectionDomain` implied `AllPermission`** | 1 |

### S11 — a security answer given the permissive way, for an unrelated reason

```text
new ProtectionDomain(null, null).implies(new AllPermission())
  HotSpot   false
  was       true
```

The native consulted CratonVM's policy core, which is **allow-all when no
`java.policy` is loaded** — and that is a correct description of the policy, not
of the domain being asked about. The JDK never asks the policy for a domain
constructed with the two-argument constructor: that constructor sets
`staticPermissions = true`, and `implies` is then

```java
if (hasAllPerm) return true;
if (permissions != null) return permissions.implies(perm);
return false;
```

A domain built with no permissions has nothing to grant, and said yes to
everything. **This is the one shape of wrong answer a permission predicate must
not produce**, and the reason it produced it had nothing to do with the domain:
the fallthrough was reached because a *different* subsystem had nothing
configured.

### S9 — the wrapping is the whole reason that overload exists

```text
doPrivileged((PrivilegedExceptionAction<String>) () -> { throw new IOException("checked"); })
  HotSpot   PrivilegedActionException wrapping java.io.IOException
  was       java.io.IOException
```

`javac` makes every caller of this overload catch `PrivilegedActionException`.
Throwing the checked exception raw sends it straight past a handler the compiler
*forced them to write* — so the failure surfaces as an undeclared checked
exception escaping a method that declares it cannot throw one.

### S8 — a null digest is worse than an exception

`digest((byte[]) null)` returned **null** rather than throwing. A null digest
compared against an expected one is a silent authentication failure, which is
the one thing this class exists to make loud.

### S1 — the message is a computation, not a constant

```text
HotSpot   Length must be at least 32 for SHA-256digests
was       partial digests not returned
```

Ten rows, one per algorithm, which is what made it obvious: the JDK's text names
the length the caller needed, and a constant string cannot. (The missing space
before "digests" is a real artefact of the JDK's own concatenation and is
reproduced rather than tidied — a caller matching on the message matches the
JDK's, not a nicer one.)

---

## 2. `jdk.internal` — 120 rows, 27 differences, and one line of them was 12

This batch is reached the only way anyone can reach it, through `--add-exports`,
with both VMs given the same flags. That is itself measured: **a VM that did not
honour `--add-exports` would fail all 120 rows at once with `IllegalAccessError`,
which is a different and larger finding than any row here.**

Two things are deliberately never called, for the same reason — the probe would
not survive its own row. `Signal.raise` DELIVERS A SIGNAL to this process, and
every signal a probe could name is one that terminates it. `VM.awaitInitLevel(n)`
BLOCKS until the VM reaches level `n`, and the VM is already at its final level
when `main` runs, so any higher `n` blocks forever; only levels at or below the
current one are asked.

| # | what | rows |
| --- | --- | ---: |
| J1 | `VM.isSupportedClassFileVersion(68, 65535)` accepted a preview minor for an old major | 1 |
| J2 | all ten `SharedSecrets.getJavaXxxAccess()` returned a NEW object per call | 10 |
| J3 | **`Signal`'s two fields were written to each other's slots** | 12 |
| J4 | `ClassLoaderValue.computeIfAbsent` stored a null mapping instead of throwing | 1 |
| J5 | a null (bootstrap) class loader was rejected as an absent argument | 1 |
| J6 | **`remove` was never registered, so it operated on a different map** | 1 |

### J3 — one line, twelve rows

```text
javap -p jdk.internal.misc.Signal
    private int number;
    private java.lang.String name;
```

The registrar's own comment said *"field 0 = name (String), field 1 = number
(Int)"* and wrote them that way. They are declared the other way round, so the
constructor put a `String` reference in an `int` slot and an `int` in a reference
slot. `getName()` came back null, `getNumber()` came back 0, `toString()` — real
JDK bytecode reading `this.name` — printed `SIGnull` for every signal, and
`equals` threw an NPE from inside the JDK.

Fixed by resolving both fields BY NAME. A hardcoded slot index against a real
class layout is a bet on declaration order, and this file had already lost that
bet elsewhere (`Throwable`'s `suppressedExceptions` carries the same scar).

### J6 — three operations on one storage and a fourth on another

`get`, `putIfAbsent` and `computeIfAbsent` are natives over a side table.
`remove(ClassLoader, Object)` was not registered at all, so real
`AbstractClassLoaderValue.remove` bytecode ran against the REAL map. The two
never met: a mapping removed through the real method stayed readable through the
natives.

**That is not a partial implementation, it is a contradiction** — and it is
invisible until someone removes something. Registering the fourth is what makes
the family coherent.

### J2 — a name that claimed an invariant the body did not keep

```text
SharedSecrets.getJavaLangAccess() == SharedSecrets.getJavaLangAccess()
  HotSpot   true      was  false
```

The allocator is called `alloc_singleton` and allocated unconditionally. The
JDK's accessors are `static final` fields set once during boot, and identity is
part of what a caller gets: code across the class library caches one in a
`static final` of its own precisely because re-fetching is supposed to be free
and to answer the same object.

**The first fix was in the wrong place, and the probe said so in nine rows.** The
memo went into `alloc_named_synthetic_singleton` — which is only the FALLBACK
arm, reached when the owner class is not loadable. Nine getters take
`alloc_singleton`'s real-class arm instead. The one getter that did get fixed
(`getJavaNioAccess`) was the one whose helper calls the fallback directly. A fix
that lands for one of ten and not the other nine is a fix at the wrong level of
the call chain, and the ratio said which.

---

## 3. What this does NOT establish

* **`ProtectionDomain.implies` now DENIES where it used to allow**, for
  statically-constructed domains with no permissions. That is the JDK's rule and
  all three arms are green on it, but it is a permission answer moving from yes
  to no, which is the direction that breaks things rather than the direction
  that hides breakage.
* **`SharedSecrets` accessors are now process-global roots.** Ten objects that
  were previously collectible now live for the life of the VM. That is what the
  JDK does with them and the count is bounded at ten, but it is a change in
  lifetime, not only in identity.
* **`AbstractClassLoaderValue.removeAll(ClassLoader)` has the same split J6
  fixed for `remove`** — it is still real bytecode over the real map while the
  other four operations use the side table. It is not in the unowned surface and
  no probe row asks it, so it is recorded here rather than changed blind.
* **`MessageDigest` provider NAMES are not compared.** `getProvider()` names
  whoever implements the algorithm and CratonVM is entitled to answer
  differently from `SUN`; what is asked is that a provider is present, that its
  name is stable, and that the digest is right.
