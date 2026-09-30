# Proposal: deliver the interpreter's JVMTI events to C agents through the per-env delivery

**Status: open — filed 2026-09-25 by interpreter round i1 wave 14, lane L1.**
A proposal, kept for triage.

## Where things stand

Wave 14 made enabling per listener on the VM's `runtime::jvmti::JvmtiEventManager`
(`docs/internal/fixed-bugs/interpreter-L1-jvmti-native-env-events-stay-enabled-on-the-vm-manager-FIXED-20260925.md`):
an attached `JvmtiEnv` receives an event only while its own state enables
it, the manager's fast-path flags are the union, and the C `jvmtiEnv`
(`vm/src/jvmti/native_env.rs`) mirrors its state into its delivery env
(`JvmtiNativeEnv::sync_event`). The C table still delivers only
`Breakpoint` and `Exception` that way (plus `VMInit`, `VMDeath`,
`ClassPrepare` on its own paths), although the manager already posts
`MethodEntry`, `MethodExit`, `SingleStep`, `FramePop`, `FieldAccess`,
`FieldModification` and `ExceptionCatch` from the interpreter, with real
`jmethodID`s (`interpreter::jvmti_method_id`), per thread, and with the JIT
standing down while any of the interpreter-only ones is enabled
(`interp_only_events_active_for_vm`).

## Proposal

Extend the C table's event set to those seven, each in three small steps
that reuse what exists:

1. `required_capability` / `POTENTIAL`: `can_generate_method_entry_events`,
   `can_generate_method_exit_events`, `can_generate_single_step_events`,
   `can_generate_frame_pop_events`, `can_generate_field_access_events`,
   `can_generate_field_modification_events`, `can_generate_exception_events`
   (already granted, covers `ExceptionCatch`) — the bit positions of
   `jvmtiCapabilities`.
2. `sync_event` mirrors each kind into the delivery env exactly as it does
   `Breakpoint` (live phase only, same thread list); the union then raises
   the manager's flag and the interpreter's guard.
3. `bind_delivery` installs one closure per event that calls the agent's
   `jvmtiEventCallbacks` member through `in_event_context`, with the
   argument conversions JVMTI specifies (`MethodExit`'s return value as a
   `jvalue`, `FramePop`'s `was_popped_by_exception`, `SingleStep`'s
   location).

Field watches need `SetFieldAccessWatch` / `SetFieldModificationWatch`
(slots 41-44) over `runtime::jvmti::set_field_watchpoint_for_vm`, and
`FramePop` needs `NotifyFramePop` (slot 20) over the thread's
`frame_pop_requests`, which must be called on a suspended thread or the
current one — the stack functions of
`docs/known-issues/interpreter/i13-L1-proposal-jvmti-c-table-stack-and-line-functions-20260925.md`
come first for that.

## Benefit

Method tracers, coverage tools and single-stepping debuggers written
against `jvmti.h` would run: today they learn at `AddCapabilities` that
nothing is available. The per-env model makes this additive — no new
flag, no new hot-path check (the interpreter's guards already exist and
already read the union).

## Staged plan

1. `MethodEntry` / `MethodExit` (the most used; the JIT stand-down is
   already wired), with a C-table test like
   `jvmti::native_env::tests::a_c_env_gets_only_what_it_enabled_and_its_disable_reaches_the_vm`.
2. `SingleStep`, `ExceptionCatch`.
3. `NotifyFramePop` + `FramePop`, then the field-watch functions and events.

Measure: the unarmed cost stays the existing union loads; with an agent
armed, a call-heavy benchmark with `MethodEntry` enabled for one thread
only, before and after, to size the per-event `deliver` snapshot (one
`Vec` of attached envs per event) — if it shows, cache the attached list
in an `ArcSwap` refreshed on attach and detach.

## Risk

Low for the VM (no new hot-path code); the callback argument conversions
are the error-prone part and each needs its own test through the C table.

## Progress (wave 17)

Interpreter round i1, wave 17, lane L2 landed stage 1 (`MethodEntry` /
`MethodExit`) in `vm/src/jvmti/native_env.rs`:

* `can_generate_method_entry_events` (bit 24) and
  `can_generate_method_exit_events` (bit 25) are in `POTENTIAL`, and
  `required_capability` asks them for events 65 / 66.
* `MANAGER_EVENTS` lists the four events delivered through the env's
  delivery env (`Breakpoint`, `Exception`, `MethodEntry`, `MethodExit`, each
  with its capability); `attach_to`, `post_vm_init` (the live transition)
  and `RelinquishCapabilities` iterate it, and `sync_event` mirrors the two
  new kinds into the delivery env exactly as it does `Breakpoint`, so the
  manager's union raises the interpreter's guards and the JIT stands down
  (`interp_only_events_active_for_vm`) only while an agent enables them.
* `bind_delivery` installs `method_entry` / `method_exit` closures calling
  `deliver_method_entry` / `deliver_method_exit`: the agent's
  `jvmtiEventCallbacks` members 15 and 16 in `in_event_context`, with the
  `jvalue` built by `return_jvalue` (a primitive in the union's low bytes, a
  reference result as a local reference; `0` for an exit by exception). The
  8-byte `jvalue` travels as a `u64` (an integer register under both 64-bit
  C conventions).
* A reference result is rooted while the callbacks run (the interpreter
  pushes it into the parent's operand stack first, or pins it:
  `fire_method_exit_keeping_value`), so an agent that calls JNI (and
  collects) in the callback gets a valid local reference.

Test: `jvmti::native_env::tests::a_c_env_gets_method_entry_and_exit_with_the_return_value`
(capability refusal, entry and exit with the `jint` result 45, the VM
interpreted while enabled, a disable and a dispose each taking the event
off).

Next: stage 2 (`SingleStep`, `ExceptionCatch`), same three steps. The
`deliver` snapshot cost with `MethodEntry` enabled for one thread is still
to be sized on a call-heavy benchmark, as the staged plan says.

## Progress (wave 22)

Interpreter round i1 wave 22, lane L1: **`SingleStep` of stage 2 landed**,
after a defect in the event's source was found and fixed.

* **The defect.** `interpreter::jvmti_events::fire_jvmti_single_step` gated
  the event on `JvmThread::single_step_enabled`, whose doc said
  `JvmtiEnv::set_event_notification_mode(Enable, SingleStep, Some(tid))`
  raises it; nothing did (only unit tests stored it). So no agent, Rust or
  C, ever received a `SingleStep`, enabled globally or per thread, while
  the JIT stood down for it all the same. The gate is now the one every
  other event has: the VM's manager and each env attached to it deliver to
  a thread only while they enable the event for it
  (`JvmtiEventManager::deliver`, `own_event_enabled`), behind the per-VM
  listener check; the field is removed. Test:
  `jvmti_events::tests::single_step_reaches_only_the_raising_vms_agent`
  (global, another thread only, this thread).
* **The C table.** `can_generate_single_step_events` (bit 16) is in
  `POTENTIAL`, `required_capability` asks it for event 60, `MANAGER_EVENTS`
  lists `SingleStep` (so `sync_event`, the live transition and
  `RelinquishCapabilities` mirror it into the delivery env), and
  `bind_delivery` installs `deliver_single_step` (member 11, `Breakpoint`'s
  signature). Test:
  `jvmti::native_env::tests::a_c_env_gets_single_step_for_every_bytecode`
  (capability refusal, a step before each bytecode of the ten-trip loop from
  0 to the `ireturn`, the VM interpreted meanwhile, a disable taking it off).

Still open from stage 2: `ExceptionCatch`, whose Rust-side callback
(`EventCallbacks::exception_catch`, `runtime/jvmti.rs`) carries no
exception object, which the C callback's `jobject exception` needs (the
interpreter has it on the handler frame's operand stack when it posts; the
manager's signature and `fire_exception_catch_for_vm` must carry it, as
`fire_exception_for_vm` carries the thrown one since wave 18). Stage 3
(`NotifyFramePop` / `FramePop`, the field watches) is unchanged; note that
`JvmThread::frame_pop_requests` is likewise never filled outside tests,
which is right until `NotifyFramePop` exists.
