# A tail call swallowed an `xreturn` that something else branched to

**Status:** ✅ **FIXED 2026-08-03** (`fix/jit-branch-target-unresolved-20260803`).
Found while validating the
[handler-body-merge fix](singlepass-codegen-refuses-handler-body-merge-FIXED-20260803.md)
— five ordinary library methods refused on BOTH arms of that A/B, so it was
pre-existing and unrelated.

## Symptom

```
[cratonvm-jitc] compile-bail org/springframework/core/ResolvableType.isAssignableFrom(…)Z
                backend_attempted=true reason=branch-target-not-an-instruction-boundary
```

Five methods, every Spring-context run, permanently interpreted:

| method |
| --- |
| `org/springframework/core/ResolvableType.isAssignableFrom(LResolvableType;ZLjava/util/Map;Z)Z` |
| `org/springframework/core/annotation/TypeMappedAnnotations.get(Ljava/lang/Class;…)LMergedAnnotation;` |
| `org/springframework/core/annotation/MergedAnnotationsCollection.get(Ljava/lang/String;…)LMergedAnnotation;` |
| `org/springframework/boot/context/properties/ConfigurationPropertiesBean.findMergedAnnotation(…)LMergedAnnotation;` |
| `net/bytebuddy/description/type/TypeDescription$ForLoadedType.getDeclaringType()LTypeDescription;` |

## The reason string was asserting a false cause

`patch_branches`'s doc comment said an unresolved target means "the bytecode
branches to a PC that is not an instruction boundary … malformed bytecode that
a classfile verifier would reject". These are javac output that HotSpot runs.
Decoding `ResolvableType.isAssignableFrom` from `javap -c`: 280 instructions,
52 branches, **0** targets off an instruction boundary.

That wrong premise is why this sat unexamined. A refusal that names a cause
must be able to be wrong out loud — the comment is now rewritten to say
"suspect a FUSION before you suspect the classfile", and the reason string
carries the target it could not resolve.

## Root cause

`note_unresolved_branch_target` (added here) prints the target and the nearest
PC the emitter actually placed:

```
branch-target-unresolved target=108 op=0xac nearest_emitted_at_or_below=105 code_len=584
branch-target-unresolved target=22  op=0xb0 nearest_emitted_at_or_below=19  code_len=23
```

Both gaps are **4** bytes from a 3-byte `invokestatic`. The walk over-advanced
by exactly one byte — the `xreturn`.

`MergedAnnotationsCollection.get` is the whole bug in 23 bytes:

```
 4: invokevirtual find(...)
 7: astore 4
 9: aload 4
11: ifnull 19
14: aload 4
16: goto 22          <-- the edge onto the return
19: invokestatic MergedAnnotation.missing()
22: areturn         <-- consumed by the tail form, never emitted
```

Source: `return (result != null ? result : MergedAnnotation.missing());`. The
`else` arm's call sits immediately before the shared `areturn`, and the `then`
arm's `goto` lands on it — the single most ordinary shape javac emits for
`?:` over a call.

Two tail-call lowerings in the `invokestatic` arm gate on "is `pc + 3` an
`xreturn` of my return type", tear the frame down and `JMP` to the callee, then
consume BOTH instructions:

```rust
pc += 3; // invokestatic
pc += 1; // xreturn
```

Neither checked whether `pc + 3` is a **branch target**. The `xreturn` is never
emitted, `pc_to_native[pc + 3]` stays `-1`, and the `goto` onto it is
unresolvable — so `patch_branches` rejects the whole method.

* the **sibling** form (`t5.2.16`, `JMP <callee entry>`) needs a
  `JitDirectCall`, i.e. the callee must already be compiled. That is why this
  never reproduced from a cold standalone probe: a hand-written driver calling
  `ResolvableType.isAssignableFrom` a million times compiles it fine, because
  its callee `ClassUtils.isAssignable` is still `callee-not-yet-compiled` when
  the caller is compiled. **A JIT refusal that depends on which callees are
  already warm will not reproduce in a one-method probe.**
* the **self-recursive** form (`JMP body_entry`) has the identical hole and is
  fixed with it.

Fusing here would be wrong on its own terms, not merely unpatchable: the other
edge arrives with its own value on the operand stack and expects a plain
return, not "load args and JMP to the callee". This is precisely the
precondition the const-arith peepholes already state in their doc comment —
*never fuse across a merge point* — which the tail-call arms never got.

## Fix

`&& !branch_targets[pc + 3]` on both `tail_op_matches` (sibling) and
`is_tail_call` (self). The call is then emitted as an ordinary CALL and the
`xreturn` is emitted normally.

One knock-on, pre-existing and correctness-preserving: a self-recursive method
that ALSO contains an `invokedynamic` refuses to compile when its recursive
call is not tail-form (the `jit-invokedynamic-groovy-regression` guard). A
self-recursive `invokedynamic` method whose recursive call sits before a shared
return therefore now stays interpreted instead of tail-calling.

## Evidence

**The lever, before any code change.** `CRATONVM_JIT=rootsnap-cache,-sp-tailcall`
on the *unfixed* binary, same Spring class: **0** unresolved targets, down from
4. That names the mechanism without trusting the reasoning.

Same base (`a64f3a5b4d`), same class, one binary apart:

| | unfixed | fixed |
| --- | --- | --- |
| `OAuth2ResourceServerAutoConfigurationTests` unresolved-target bails | 4 | **0** |
| its `full-compile` count | 3754 | 3797 |
| `JettyServletWebServerFactoryTests` unresolved-target bails | 1 | **0** |
| the five methods | interpreted forever | all `full-compile`, C1 and C2 |
| `cargo test -p cratonvm-jit` | 1832 pass | 1834 pass + 3 new |
| both Spring classes | PASS | PASS (52/52, 113/113) |
| `regression-suite/run.sh` | — | 22 passed, 0 failed |

No other bail reason moved: the rest of both histograms is unchanged apart from
tiering nondeterminism.

The three fixtures are real guards — on the unfixed tree
`a_self_tail_call_may_not_swallow_a_branch_targeted_return` and
`a_sibling_tail_call_may_not_swallow_a_branch_targeted_return` both FAIL while
`a_tail_call_over_an_unshared_return_still_compiles` passes, so they isolate the
merge, not the tail call.
