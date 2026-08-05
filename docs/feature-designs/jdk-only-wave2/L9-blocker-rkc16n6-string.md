# L9 — Blocker: real `java/lang/String` bytecode during JDK `<clinit>`

**Owns:** `vm/src/runtime/interpreter/` (resolution path)
**Gated on:** nothing — but **it gates L11**, so start it early.
**Effort:** L
**Note:** this is **not a jdk-only change.** It is a VM defect that jdk-only
work is waiting on, and it should be scheduled and staffed as such rather than
counted as jdk-only progress.
**Evidence:** [`forced-native-string-policy-two-lists-that-disagree.md`](../../known-issues/jdk-only/forced-native-string-policy-two-lists-that-disagree.md)

## Goal

Until real-JDK `java/lang/String` bytecode resolves correctly during JDK
`<clinit>`s, both forced-native `String` lists have to stay:

* a **21-name positive list** on the cold path;
* a **7-pair exclusion** on the warm path;
* plus a JIT direct-call ladder.

The disagreement between them already turned a landed, measured h2-bnf
performance fix into **statically unreachable code** — a fix that had never once
executed.

## Why it is the blocker, precisely

The lists exist because during early `<clinit>`s the VM cannot yet run real
`String` bytecode, so it must force its own natives. Every proposed deletion of
those lists ends at "let `resolve_dispatch` decide from `NativeKind` +
`Method::code()`" — which is only safe once the real bytecode is reachable at
that point in boot.

So this lane's deliverable is not "delete the lists" (that is L11). It is
**make real `String` bytecode work during JDK `<clinit>`**, at which point L11
becomes a deletion rather than a redesign.

## Steps

1. Reproduce: identify the earliest `<clinit>` that needs `String` and fails
   without the forced native. `CRATONVM_DBG=jdk-only` plus the trace flags will
   name it.
2. Establish *why* it fails — missing `<clinit>` ordering, an uninitialised
   static, or a native that the real bytecode depends on. The record's RKC16N.6
   reference is the starting point.
3. Fix, then confirm the two lists are no longer load-bearing by **removing them
   and booting**, not by argument. Restore them if the boot fails; the point of
   the experiment is the boot, not the diff.

## Verification

* Strict boot with both lists removed.
* Both probes vs HotSpot in both modes.
* The h2-bnf performance fix must actually execute — it is the canary. Confirm
  with a counter, not by reading the code; it was statically unreachable for
  months without anyone noticing.
* Full suite run: `String` is on every path, so this is the change most likely
  to move something far away.

## Done when

Real `String` bytecode runs during JDK `<clinit>`s, the lists are provably
unnecessary (removed, boots clean), and L11 is unblocked.
