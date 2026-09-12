# A hidden class naming itself: the door census — 2026-09-12

The 2026-09-12 fix for hidden-class self-reference covered the doors ONE program
reached (`MethodHandleProxies`' generated proxy, four self-references). This is
the census of every door, taken with a purpose-built instrument against the
HotSpot oracle.

**The instrument and the measurement land here. The remaining fix does not** —
§4 names the door and §5 says what is left.

## 1. The instrument

`probes/L2HiddenSelfRef.java` emits, with the `java.lang.classfile` API, a class
that names ITSELF through every opcode that can carry a `CONSTANT_Class`, defines
it with `Lookup.defineHiddenClass(bytes, true)`, and drives one row per door
through the returned `Lookup`. Every row COMPUTES a value only correct resolution
can produce — `ldc` must be the SAME `Class` object (`==`, not name equality, since
a hidden class has no findable name), fields round-trip `0x5A5A`, the three invoke
forms return distinct constants, `instanceof` carries a `String` negative control.

`probes/L2HiddenSelfRef-runner.sh` runs **one row per PROCESS**, and that is not
tidiness. See §3.

## 2. Two of the rows were MY misunderstanding, and the oracle said so first

Run against HotSpot 25.0.4+7 before being asserted, two rows failed **on the
oracle**:

```
FAIL row multianewarray: java.lang.NoClassDefFoundError: [[LL2SelfRefVictim;
FAIL row ldcMethodType:  java.lang.NoClassDefFoundError: L2SelfRefVictim
```

Both are correct JVMS behaviour. `multianewarray`'s `CONSTANT_Class` is the ARRAY
type `[[LName;` and a `CONSTANT_MethodType` is a DESCRIPTOR — both resolved BY
NAME, and a hidden class has no findable binary name, so no descriptor can name
one. Single-dimension `anewarray` passes for the opposite reason: its
`CONSTANT_Class` is the COMPONENT, i.e. the hidden class's own entry. (This is
also why `MethodHandleProxies`' proxy `ldc`s a MethodType over the INTERFACE's
descriptor and never over its own.)

Both rows are now asserted as REFUSALS rather than deleted, so a VM that "fixes"
them by resolving a name HotSpot refuses fails here. **Had I scored this probe
without an oracle I would have gone and patched two resolution paths into
accepting names the JDK rejects** — a wrong answer, where the bug being fixed is
only a missing one.

## 3. One row per process, because the failure is not a Java throwable

CratonVM reports an unresolvable constant-pool class inside a hidden class as

```
MethodCallFailed::InternalError(VmError::ClassFile(ClassFileError::ClassNotFound))
```

which is **not** a Java throwable. `catch (Throwable)` in the harness never sees
it; the VM prints `class file error: class not found: L2SelfRefVictim` and the run
dies. In-process, the first casualty hid the other eleven rows and the run looked
like it simply stopped — no tally, no failures listed. Hence one row per process.

That is a finding in its own right: JVMS §5.4.3.1 makes an unresolvable class
reference a `NoClassDefFoundError`, which Java code can catch. Here it is
uncatchable. The conversion boundary exists
(`vm/src/runtime/exceptions.rs`, `raise_no_class_def_found`, with a
`CRATONVM_DBG_LINKAGE_BT=1` hook) — and the failing door does not go through it:
that hook prints nothing on this failure.

## 4. The census, and the single door behind it

Compatible mode, `cvm-dc0-trial` (dev `0a805f3fc` + the landed fixes):

| row | door | HotSpot | CratonVM |
|---|---|---|---|
| `runAll` | executes at all | PASS | PASS |
| `ldcIdentity` | `ldc <ITSELF>` | PASS | PASS |
| `newOnly` | `new <ITSELF>` alone | PASS | PASS |
| `staticField` | `putstatic`/`getstatic <ITSELF>` | PASS | PASS |
| `invokeStatic` | `invokestatic <ITSELF>` | PASS | PASS |
| `arrayComponentType` | `anewarray <ITSELF>` | PASS | PASS |
| `newAndInit` | `invokespecial <ITSELF>.<init>` | PASS | **FAIL** |
| `newAndField` | …then `getfield` | PASS | **FAIL** |
| `invokeSpecialPrivate` | …then `invokespecial priv()` | PASS | **FAIL** |
| `invokeVirtual` | …then `invokevirtual virt()` | PASS | **FAIL** |
| `anewarray` | …then `aastore`/`getfield` | PASS | **FAIL** |
| `checkcast` | …then `checkcast <ITSELF>` | PASS | **FAIL** |
| `instanceOf` | …then `instanceof <ITSELF>` | PASS | **FAIL** |
| `multianewarrayIsRefusedByTheJdkToo` | refusal expected | PASS | **FAIL — resolves it** |
| `ldcMethodTypeIsRefusedByTheJdkToo` | refusal expected | PASS | **FAIL — wrong type** |

**It is ONE door, not six.** `newOnly` isolates `new <ITSELF>` and passes;
`newAndInit` adds only `invokespecial <ITSELF>.<init>` and fails. Every other
failing row constructs an instance first, so they are all the same casualty — and
the two rows that pass while touching arrays (`arrayComponentType`) are exactly
the ones that never construct one. A census that stopped at "six doors are open"
would have sent six fixes after one bug.

The two remaining failures are independent WRONG ANSWERS, not the same door:

* `multianewarray [[L<ITSELF>;` — CratonVM **resolves** it where HotSpot refuses.
  Permissive in the direction that matters: it is answering a descriptor that
  names a class no loader can find.
* `ldc` of a `MethodType` naming itself — CratonVM raises
  `TypeNotPresentException`, HotSpot raises `NoClassDefFoundError`.

## 5. What is left, and where to start

The failing door is the `invokespecial` owner resolution for a constructor on the
hidden class itself. It does not go through `resolve_class_loader_aware` (whose
`hidden_self_reference` arm is what makes `ldc`, `new`, `getstatic` and the field
doors pass), and it does not go through the `NoClassDefFoundError` conversion
boundary either.

The lead, from the error's own construction sites: `ClassIdentityError::Refused`
in `native-api/src/class_identity.rs` maps to
`ClassFileError::ClassNotFound`, i.e. a class-identity POLICY refusal that reads
as absence. That module's own doc comment says it exists to undo exactly this
collapse ("Mapping both onto `ClassNotFound` would put ambiguity back on top of
absence"). Start by confirming whether the hidden name is being refused there
rather than merely missed — `CRATONVM_DBG_LINKAGE_BT=1` prints nothing on this
failure, which already rules out the resolver that hook instruments.

`invokespecial` routes `0xb7` -> `invoke_fast::execute_nonvirtual_fast_door` ->
`execute_invokevirtual_cached` (`dispatch_virtual.rs`) -> `execute_invoke`
(`invoke.rs`); none of those three files resolves a CP owner name through
`resolve_class_loader_aware`, and `invoke.rs`'s `self_match`
(`resolved_private_invokevirtual_target`) is already hidden-aware but is only
consulted for a PRIVATE invokevirtual, not for `<init>`.

The predicate to reuse is
`crate::runtime::interpreter::constants::is_self_class_reference(stored, hidden, referenced)`.
