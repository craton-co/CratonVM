# i132-L6: an `invokespecial` from an interface skips HotSpot's receiver subtype check

Interpreter round i6, wave 132, lane L6. Found while proving stage 1 of
`docs/internal/proposals/interpreter/i40-L2-proposal-guard-a-pinned-private-interface-call-instead-of-refusing-the-method-20261004.md`.
Status: OPEN (parity defect, both modes; hand-assembled bytecode only). Not built.

## Symptom

`tools/probes/interp/L6/L6W132PrivateIfaceThisPin.java`, row `special-bad`. An interface
`W132Sp` has a default `m(Object)` whose body is `aload_1; invokespecial W132Sp.priv()`; the
argument is a `String`. HotSpot 25.0.3 (default and `-Xint`) raises, on every call:

    java.lang.IllegalAccessError: Receiver class java.lang.String must be the current class or a subtype of interface W132Sp

CratonVM has no such check (no source line carries the message). By reading, it runs `priv`
with a `String` as `this`; `priv`'s own `invokeinterface W132Sp.priv2` on `this` then raises the
JVMS §6.5 `IncompatibleClassChangeError` ("does not implement the requested interface"), so the
row shows the wrong error type and message. With `CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN=1` and
`priv` compiled, `priv2` runs and the call returns `"p2"`: no error at all.

javac never emits the shape (a receiver other than `this` for an `invokespecial` from inside an
interface); the verifier accepts it because it treats interface types as `Object`.

## HotSpot's rule

`LinkResolver::runtime_resolve_special_method` (JDK 25): for a method other than `<init>`, when
the SENDER (the class holding the `invokespecial`) is an interface and the receiver is not null,
the receiver's class must be the sender or a subtype of it; otherwise `IllegalAccessError` with
the message above (`external_name`s, dots). The verifier proves the same for a class sender, so
only interface senders are checked at run time.

## What would fix it

1. The interpreter's `invokespecial` (`vm/src/runtime/interpreter/invoke.rs`, the special arm,
   lane L4's file): after the null check, when the calling class is an interface and the method
   is not `<init>`, test `receiver class is-subtype-of calling class` and raise the
   `IllegalAccessError` with HotSpot's text. Both modes (a parity fix; a `HotSpotAlignment` row
   if an escape is wanted).
2. The compiled `invokespecial` sites in interface methods (`special_site_bind_refusal`,
   `jit_bridge.rs`): refuse the bind unless the receiver is the method's unstored `this`; the
   proof is `jit_bridge::private_iface_this::every_site_receiver_is_this` (wave 132), reusable
   with the opcode generalised.
3. Then `L6W132PrivateIfaceThisPin` `special-bad` is SAME in both modes, JIT and `--nojit`, with
   `CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN` off and on.

Until (1) and (2) exist, `CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN` must stay off: its proof assumes
that `this` of an interface instance method implements the interface, which this check is what
guarantees.

## Wave 133 (lane L4), 2026-12-31

Status: FIX WRITTEN, NOT BUILT (`CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK`, parity, both modes,
default ON, `=0` off). Move to `docs/internal/` once the commands below pass.

1. Interpreter (`vm/src/runtime/interpreter/invoke.rs`, `execute_invoke_kind`): for an
   `invokespecial` other than `<init>` whose sender is an interface, a non-null receiver whose
   class is not the sender or a subtype of it raises `IllegalAccessError` with HotSpot's text
   (`interface_special_receiver_refusal`), before the §6.5 selection (so before an
   `AbstractMethodError`, as in `runtime_resolve_special_method`). Such a site is never cached
   (the three `populate_invoke_cache` fills of the slow path skip it), as HotSpot never marks it
   resolved, so every execution goes through the check; the fast doors only serve cached entries.
   Cost: the slow path for `I.super.m()` sites and pre-JDK-11 private interface calls, plus one
   class-manager read per interface-sender `invokespecial` slow call.
2. Compiled doors (`jit_bridge.rs`, `special_site_bind_refusal_uncounted`): refuse
   `invokespecial-interface-receiver-unchecked` unless every `invokespecial` of that member, in
   every method of the interface, pushes its instance method's unstored `this`
   (`interface_special_receivers_proven`, using
   `private_iface_this::every_special_site_receiver_is_this`, the wave-132 walk generalised to
   `0xb7`). The question is per member, not per site, because the refusal function receives no
   pc. javac's shapes (`I.super.m()`, private calls on `this`) are proven and still compile;
   `special-good` (receiver is local 1) is refused, so its `m` stays interpreted.
3. Pass criteria: `tools/probes/interp/L6/L6W132PrivateIfaceThisPin.java` row `special-bad`
   equals the `HOTSPOT_EXPECTED` block (`ok=0 icce=30000 other=0 first=java.lang.IllegalAccessError:
   Receiver class java.lang.String must be the current class or a subtype of interface W132Sp`)
   and `special-good` stays `ok=30000 icce=0 other=0`, in `--jdk-only` and `--compatible`, JIT
   and `--nojit`, with `CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN` unset and `=1`. With
   `CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK=0` the old DIFF returns. Unit test:
   `cargo test -p cratonvm-vm special_sites_on_this_are_proven`.

Once this is built and green, the precondition this page set for
`CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN` holds; the flip itself is the switch owner's call.

## Wave 134 (lane L4), 2026-10-19

Status: FIX WRITTEN, NOT BUILT (review of w133, doubtful items 1 and 2). Still OPEN until the
wave-133 criteria above and the runs below pass; then move to `docs/internal/fixed-bugs/`.

1. The switch is now a per-VM field, `VmConfig::interface_special_receiver_check`
   (`vm/src/config.rs`, `runtime_flag_default_on`, read once when the configuration is built;
   default ON, `=0` off, unchanged). `invoke::interface_special_receiver_check_enabled(shared)`
   reads the field; the slow path tests it FIRST, before the `<init>` compare and the
   class-manager read, and the compiled doors' refusal reads the same field before their bytecode
   walk. No environment read remains per execution.
2. Lambda proxies: `interface_special_receiver_refusal_for_class` answers `None` for a VM-minted
   proxy in an explicit arm, before any `ClassStore` lookup (HotSpot's proxy implements the
   functional interface and its superinterfaces, and javac's only receiver there is `this`). It
   no longer passes by the accident of `get_class(receiver)?` returning `None`.
3. Caching a positive verdict per (site, receiver class): not done. It would be parity-safe (a
   class's supertypes never change), but it needs new per-site state on a path that is uncached by
   design, which is performance work, and wave 134 is defects only. The check itself is one
   class-manager read and an `is_subclass_of` walk, small against the uncached slow path it sits
   in. The real cost is the uncached site, which HotSpot also pays (it re-resolves every call).

Runs (orchestrator):

- `tools/probes/interp/L4/L4W134IfaceSuperLoop.java` (new; `I.super.m()` from a subinterface on
  a class instance, a lambda proxy and an anonymous class, 300000 calls each): `--jdk-only` and
  `--compatible`, JIT and `--nojit`, output equals its `HOTSPOT_EXPECTED` block (TIME lines
  excluded); also with `CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK=0`.
- `CRATONVM_DBG_FLAGREADS=1 cratonvm --jdk-only --nojit -cp . L4W134IfaceSuperLoop`: no report
  row names `CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK` (before: one read per call, 900000 reads).
- `tools/probes/interp/L6/L6W132PrivateIfaceThisPin.java` rows `special-bad` and `special-good`
  as in the wave-133 criteria, both modes, JIT and `--nojit`.
- `cargo test -p cratonvm-vm i134_l4_interface_special_receiver_tests` and
  `cargo test -p cratonvm-vm the_interface_special_receiver_check_switch_defaults_on_with_a_zero_escape`.
