# Continue: CratonVM HashMap iteration order ≠ HotSpot — gauntlet-wide test fragility

**Severity:** medium (cross-cutting). Java does not specify `HashMap`/`HashSet` iteration order, but a large
fraction of real JDK-targeting tests/apps depend on **HotSpot's specific** order. CratonVM's `HashMap`
iterates differently, so any code that (a) builds an error/diagnostic string by iterating a `HashMap`, (b)
serializes a `HashMap`/Jackson `ObjectNode`, or (c) asserts on a collection's encounter order, can diverge.

## Concrete confirmed repro (small, isolated)
Keycloak SD-JWT `DefaultCryptoSdJwtVerificationTest.sdJwtVerificationShouldFail_IfDuplicateSaltValue`
(see `continue_prompt_sdjwt_verify_ordering.md` for the full run recipe). It throws the right
`IllegalArgumentException`, but the message orders two claims by `HashMap` iteration:
CratonVM `'given_name' and 'family_name'` vs HotSpot `'family_name' and 'given_name'`
(`org.keycloak.sdjwt.DisclosureSpec.Builder.undisclosedClaims = new HashMap<>()`). Likely the same root cause
behind `testSdJwtVerification_RecursiveSdJwt` (nested-disclosure JSON field order → different digest).

## Task
1. **Scope the impact first** before changing anything: grep the gauntlet for tests that assert on
   `HashMap`/`HashSet`/`keySet`/`entrySet`/`values`/`ObjectNode` encounter order, and quantify how many
   currently-failing app/JUnit cases are pure ordering mismatches (not logic bugs). This decides whether it's
   worth matching HotSpot.
2. If worth it, make CratonVM's `java.util.HashMap` reproduce HotSpot's iteration order **exactly**:
   - same `hash()` spread: `h = key.hashCode(); h ^= (h >>> 16)`;
   - same initial capacity / load factor (16 / 0.75) and **same resize doubling + bin split** (preserve/relocate
     order HotSpot uses on resize);
   - same bucket walk order on iteration (index 0..n, then chain insertion order / tree order);
   - treeify at 8 / untreeify at 6 with HotSpot's comparator fallback.
   Then mirror it for `LinkedHashMap` (insertion/access order — should already match; verify) and ensure
   Jackson `ObjectNode` (LinkedHashMap-backed) preserves insertion order.
3. Verify against a broad set: the two SD-JWT tests above, plus re-run the app gauntlet / JUnit suites for
   regressions (ordering changes can fix some and break others if any code accidentally depended on the old
   order).

## Where
CratonVM `HashMap` lives in `native-collections/` (+ any `java/util/HashMap` intrinsics in `native-builtins`).
Confirm whether `HashMap` is real JDK bytecode or a CratonVM intrinsic in the real-JDK (`legacy-synthetic-crypto`,
no `synthetic-jdk`) build via `cratonvm --dump-native-registry <file> <class>` — if intrinsic, the order is
defined by the Rust impl; if real bytecode, the order is defined by `key.hashCode()` + the array layout the VM
gives `HashMap.table`.

## Caveat
This is genuinely unspecified behavior; some maintainers prefer to fix the *tests* rather than the VM. Get a
direction call before a large `HashMap` rewrite. Build/run with the **renamed-binary** trick from
`continue_prompt_sdjwt_verify_ordering.md` (a parallel agent loops `taskkill //IM cargo.exe|rustc.exe|cratonvm.exe`).
