# WORKER-3-NOTE-5 — the census has a documented exemption, and nobody has priced it

**2026-08-22**, on `d235cced9`. All figures MEASURED from
`--dump-native-registry` and from the dispatch source; no suite run is involved,
so host load does not bear on any of it.

`WORKER-3-NOTE-4` §4.5 ended with a claim I owed evidence for: that the census
may be measuring one registrar out of several. That claim was **wrong in its
framing and worse in its conclusion.** The census is not missing these rows by
accident. It excludes them **by an explicit, deliberate condition**, and the
excluded population has never been counted.

## 1. The exemption, in the source

`NativeKind::Intrinsic` is skipped at every one of the three sites that can
record a `native-shadows-bytecode` row (`vm/src/vm/vm_exec.rs`, the only
producer in the tree outside the JIT's thin-helper refusal):

```rust
// vm_exec.rs:1405
if policy.is_jdk_only() && bytecode_available && kind != NativeKind::Intrinsic {
    record_native_shadows_bytecode(class_name, method_name, descriptor, kind);
}

// vm_exec.rs:1429 — §1.4, "the reviewed exception; may shadow bytecode."
NativeKind::Intrinsic => Some(DispatchDecision::Intrinsic(callback)),
```

and in `resolve_step1_native`, step 2 returns the intrinsic **before** step 3,
which is the arm that records. Step 3's own comment states the consequence:

> *Step 2 above has already consumed every `Intrinsic`, so anything still here
> is a `Bridge` or a `SyntheticStub`.*

So the 1387-row population is, by construction, **`Bridge` + `SyntheticStub`
only**. That is a defensible design — a genuinely reviewed intrinsic is what
every JVM does with `Math.sqrt` — but it is an exemption, and an exemption's
size is a number somebody has to publish.

## 2. The size of it

MEASURED, one `--jdk-only --dump-native-registry` run:

| | rows |
|---|---:|
| registrations, total | 10832 |
| `bridge` | 10203 |
| **`intrinsic`** | **629** |
| `intrinsic` **and** `owns_slot: true` | **595** |
| …**and the real JDK 25 class declares the method WITH CODE** | **398** |
| …`acc_native` (a legitimate native binding) | **1** |
| …not declared by the image at all (the "fourth verb", inside the exemption) | **193** |

**398 registrations are shadows by §1.4's own definition and are exempt from the
count by construction.** Against a published population of 1387 native-won rows,
that is **+29% on the denominator**, none of it visible in any figure in
`docs/known-issues/jdk-only/`.

**305 of the 398 are `java/lang`** — this lane's block.

| class | exempted shadows |
|---|---:|
| `java/lang/Math` | 69 |
| `java/lang/StrictMath` | 69 |
| `java/lang/Character` | 36 |
| `java/lang/String` | 23 |
| `java/lang/Integer` | 21 |
| `java/lang/Long` | 20 |
| `java/math/BigDecimal` | 19 |
| `java/lang/Double` / `java/lang/Float` | 15 each |
| `java/math/BigInteger` | 15 |

By package: `java/lang` 305, `java/util` 50, `java/math` 34, `jdk/internal` 6.

## 3. The exemption is not benign — one member returns a wrong answer

`Math` and `StrictMath` are 138 of the 398 and are exactly what an intrinsic
exemption is for. That is the strongest case for the design, and it does not
generalise.

The native at the centre of the `H22` StringBuilder catastrophe —
`WORKER-3-NOTE-4` §4.4, the one that makes `toString()` return `""` — is itself
a member of this population. MEASURED from the same dump:

```text
class=java/lang/String  name=<init>  desc=(Ljava/lang/AbstractStringBuilder;Ljava/lang/Void;)V
  kind=intrinsic  owns_slot=True  registered_by=native-builtins/src/lib.rs:11260
  real: declared=True  has_code=True  acc_native=False
  => census-exempt shadow: True
```

It is registered `NativeKind::Intrinsic` **deliberately**, and its registration
comment says why — *"`Intrinsic` for the same load-bearing reason: this VM's
builders are `char[]`-backed"*. The kind was chosen to make a layout
incompatibility work, and the census exemption came along with it.

So the exemption contains at least one registration that:

* shadows real bytecode,
* returns a **silently wrong answer** on the real layout (`""` for a builder
  holding `abc7`, `rc=0`, no exception), and
* **cannot appear in any census figure**, now or in any past wave.

"Reviewed" is doing a lot of work in that comment. 398 rows carry the label; the
review that named them is not cited anywhere I can find, and this one would not
survive it.

## 4. What this changes

* **The contract's headline number is a floor for a second, independent
  reason.** `run.sh` already warns that a truncated report makes the counts
  floors. This is structural rather than incidental: 398 rows can never be
  counted regardless of report capacity.
* **`H14-2`'s "completion is roughly 5%" is optimistic**, since its denominator
  omits 398 shadows — 305 of them in the two `java.lang` blocks this brief
  assigns to WORKER 3.
* **`NativeKind::Intrinsic` is a way to make a row disappear from the census
  without changing behaviour.** Nothing in the tree prevents a future wave from
  clearing rows by re-tagging them `Intrinsic`, and it would score as progress —
  the same failure mode as `WORKER-3-NOTE-4` §4.3, at the level of the metric
  rather than the hierarchy.

## 5. What to do, in order

1. **Publish the exemption.** Any figure quoting the shadow population should
   carry `+398 exempt` beside it. One line in `run.sh`'s census block; the count
   is already computable from the registry dump.
2. **Ratchet the kind.** `Intrinsic` should not be assignable without a cited
   review. A test that fails when the `Intrinsic` count rises would stop the
   re-tagging route before anyone takes it.
3. **Re-review the 260 non-math rows.** `Math`/`StrictMath` (138) are
   defensible on sight. `String` 23, `Character` 36, and the boxes (71) are the
   classes where this VM's layout assumptions have already produced live wrong
   answers, and they have never been audited as a group.
4. `String.<init>(AbstractStringBuilder,Void)` specifically is a known-wrong
   member; it is the blocker in `WORKER-3-NOTE-4` §4.5 and should be fixed or
   re-kinded, not left labelled reviewed.

## Index rows for `INDEX.md` (H0 to place)

* `WORKER-3-NOTE-5` — the census excludes `NativeKind::Intrinsic` by explicit
  condition at all three recorder sites; **398 registrations shadow real
  bytecode and can never be counted**, +29% on the 1387 denominator, 305 of them
  `java/lang`
* `WORKER-3-NOTE-5` §3 — the exemption is not benign: the native that empties
  every `StringBuilder` under the real layout is itself `kind=intrinsic` and
  census-exempt
* `WORKER-3-NOTE-5` §4 — re-tagging a row `Intrinsic` removes it from the
  census without changing behaviour, and nothing in the tree prevents it
