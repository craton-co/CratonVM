# L2 — the lane brief, COMPLETED 2026-08-28

**This is the retired assignment, not a record of the work.** The work is in
`l2-strings-eighteen-defects-five-root-causes-and-the-writer-half` (this folder)
and its three open residuals are in `l2-strings-residuals-the-migration-is-unpriced`
under `docs/known-issues/jdk-only/`.

**Every question the brief asked was answered, and the answers are worth carrying
forward because four of them were predictions that came true:**

| the brief said | what was measured |
| --- | --- |
| "`StringBuffer` is synchronized; `StringBuilder` is not. Every mutator has a different thread-safety contract." | Two threads appending 4000 characters each to one `StringBuffer` finished with FEWER than 8000, no exception. Fixed by retiring all 62 `StringBuffer` registrations so its own `synchronized` bodies run. |
| "`StringBuffer` also caches a `toStringCache` that must be invalidated on EVERY mutation — a missed invalidation is a stale `toString()` with no exception anywhere, which is the quietest failure shape in this whole surface." | Exactly that, and it appeared only AFTER the retirement, for the six `insert` overloads whose bodies delegate without nulling the cache because the JDK re-dispatches virtually to reach the one that does. Fixed at `sb_set_count`. |
| "`AbstractStringBuilder` is package-private and abstract, so anything registered on it runs for BOTH subclasses… the same hazard applies here." | It is the mechanism of the row above. A native on the abstract base renders the value itself and never re-enters, so the subclass's hook never fires. |
| "`append(null)` for `String`, `CharSequence` and `char[]` — **the three differ**" | They do, and only the `char[]` one was wrong: it was a silent no-op where HotSpot throws. Two of the ten null contracts. |
| "`capacity()` / `ensureCapacity` / `trimToSize` growth is observable and specified" | Correct before and after; the growth rule is now reproduced rather than approximated, and six probe rows pin it. |
| "`reverse()` over surrogate pairs", "`appendCodePoint` with an invalid code point", "`chars()` / `codePoints()` over a lone surrogate", "`insert` / `delete` / `replace` / `setLength` with out-of-range and reversed indices" | `reverse`, `appendCodePoint` and every bounds edge were ALREADY correct. `chars()` / `codePoints()` were not — but not for the surrogate reason the brief expected: they were wrong for every character above U+00FF, including a lone surrogate, because they read the payload as LATIN1. |
| "CHECK THIS BEFORE YOU START: `WORKER-3-NOTE-3` has the StringBuilder cluster open with a diagnosed mechanism… talk to that lane, or take the surrounding rows first if it is still live." | Checked. Nobody was on it, and its N1 residual — the object-model migration — is the root cause of three of this lane's defects, so this lane took it. That note's N1 is now closed; its other nominations are untouched. |

**What the brief got wrong, and it is worth saying:** it sized the lane at 176
bridge-with-code rows and told the worker to take the `native-won` triples from
the report. That list cannot contain a method with no native — and `chars`,
`codePoints` and `compareTo`, three of the family's four worst defects, have no
native at all. The report is a lower bound on a family's surface, not a
description of it.

---

# L2 — `StringBuilder` / `StringBuffer` / `AbstractStringBuilder`: 176 rows

**Read `HANDOFF-20260828-SCOPE.md` first.**

**Owner: TAKEN 2026-08-28**, branch `claude/l2-strings-20260828`, worktree `/data/cvm-l2s-20260828` on the Linux build host. This lane also took `WORKER-3-NOTE-3` — nobody was on it, and its N1 residual turned out to be the root cause of four of this lane's measured defects. (Superseded text: unclaimed.) L5 (`claude/jdk-only-mode-handoff-09b48c`, worktree
`h2-known-issues-206dee`) is the only lane currently running.

## Your families

```text
java/lang/AbstractStringBuilder   61 bridge-with-code rows
java/lang/StringBuffer            58
java/lang/StringBuilder           57
                                  ---
                                  176   (8%)
```

Registrar: mostly `native-builtins/src/lang_string.rs` (16 762 lines), which is
comparatively self-contained — **this lane has the least collision risk with the
others** and is a good one to run alongside anything.

## CHECK THIS BEFORE YOU START

`WORKER-3-NOTE-3` has the StringBuilder cluster **open with a diagnosed
mechanism**. Find and read that note. This campaign deliberately kept out of the
176-row cluster for that reason. Do not re-derive or contradict it — talk to that
lane, or take the surrounding `StringBuffer` / `AbstractStringBuilder` rows
first if it is still live.

## The shape to aim at

Three classes sharing one implementation is exactly the "shared surface" pattern
that has paid out **three times** in this campaign (`Inet4/6AddressImpl`,
`PKCS12`/`JKS`, `Atomic*FieldUpdater` impl+base). The registrar almost certainly
carries a comment claiming they are the same. They are not:

* **`StringBuffer` is synchronized; `StringBuilder` is not.** Every mutator has a
  different thread-safety contract. `StringBuffer` also caches a
  `toStringCache` that must be invalidated on EVERY mutation — a missed
  invalidation is a stale `toString()` with no exception anywhere, which is the
  quietest failure shape in this whole surface.
* **`AbstractStringBuilder` is package-private and abstract**, so anything
  registered on it runs for BOTH subclasses. Registering an accessor on an
  abstract base shadowed a user subclass in `Atomic*FieldUpdater` this campaign;
  the same hazard applies here, even though the subclasses are the JDK's own.
* `capacity()` / `ensureCapacity` / `trimToSize` growth is observable and
  specified.

## Edges worth asking

* `insert` / `delete` / `replace` / `setLength` with out-of-range and reversed
  indices;
* `deleteCharAt` at the boundary, `setCharAt` at exactly `length()`;
* `appendCodePoint` with an invalid code point;
* `reverse()` over surrogate pairs — the JDK preserves pairs rather than
  reversing the code units;
* `chars()` / `codePoints()` over a lone surrogate;
* `append(null)` for `String`, `CharSequence` and `char[]` — **the three
  differ**: two append the text "null", the `char[]` overload throws NPE.
