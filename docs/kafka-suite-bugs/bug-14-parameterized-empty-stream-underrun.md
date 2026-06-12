# Bug 14 — `@ParameterizedTest` argument provider yields empty stream → massive test under-run

**Severity:** High — silently runs **far fewer tests** than HotSpot (e.g.
`FetchCollectorTest` ran 22 vs HotSpot's 129). The missing tests never execute, so
the failure is invisible in pass/fail counts unless compared to the baseline.
Reproduces under `--nojit`. HotSpot clean.

## Symptom
```
=> org.junit.platform.commons.PreconditionViolationException: None of the supporting
   TestTemplateInvocationContextProviders [ParameterizedTestExtension] provided a
   non-empty stream
```
A `@ParameterizedTest` whose arguments come from a `@MethodSource` /
`@EnumSource` / `@ValueSource` provider gets an **empty** argument stream on
CratonVM, so JUnit either errors (the precondition above) or simply generates zero
invocations → the class reports a much smaller `found` count than HotSpot.

## Root cause (to pin down)
The argument-provider `Stream`/`Arguments` is empty on CratonVM. Candidate causes:
- the `@MethodSource` static method returns a `Stream`/`Collection` that a CratonVM
  stream/`Collectors` intrinsic flattens to empty, or
- reflective discovery of the provider method returns nothing, or
- an `EnumSource`/generated-enum `values()` returning empty (cf.
  [bug-12](bug-12-nodeapiversions-apikey-npe.md)).

Reproduce by running the specific `@MethodSource` provider method directly under
CratonVM and checking the element count vs HotSpot.

## Affected classes (partial — append more later)
- consumer.internals.FetchCollectorTest (found 22 vs hs 129 — UNDERRUN)
- (any class with `cv found < hs found` in compare.sh is a candidate — append from
  the full run)
