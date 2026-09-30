# Proposal: `InstanceOnly` on breakpoints, steps and method events, and `canUseInstanceFilters`

**Status: proposal — filed 2026-10-04 by interpreter round i1 wave 40, lane
L1, from the area review. Not implemented.**

## What is missing

`VirtualMachine.CapabilitiesNew` answers `canUseInstanceFilters` (index 11)
false (`debug::commands::handle_vm_capabilities_new`), so JDI's
`addInstanceFilter` on every event request throws
`UnsupportedOperationException` (`EventRequestManagerImpl`'s `addInstanceFilter` checks
`vm.canUseInstanceFilters()` first). HotSpot answers true. An IDE's
"instance filter" on a breakpoint (IntelliJ: *Instance filters*; Eclipse:
*Instance breakpoint*, "Stop only in selected object") is therefore
unavailable against CratonVM.

What exists today: `EventModifier::InstanceOnly` (JDWP modKind 11) is read
and accepted on field watch requests only (`commands::handle_er_set`,
wave 10), where the instance is the object whose field is accessed; on any
other kind it is refused with `ILLEGAL_ARGUMENT`, and the location-event
matchers (`EventManager::match_location_events`, `match_subject_events` and the class-event matcher in
`vm/src/debug/events.rs`) answer `false` for it outside the field watches. Since JDI never sends it
without the capability, even the field-watch arm is unreachable from JDI.

## The proposal

1. **Match `InstanceOnly` on the location events** — `Breakpoint`,
   `SingleStep`, `MethodEntry`, `MethodExit` / `MethodExitWithReturnValue`,
   `Exception` (the throwing frame), and the field watches as today: the
   instance is the `this` of the frame the event is reported from (local 0
   of an instance method; a static method has no instance and never
   matches — to be checked against HotSpot with the probe below). The suspend
   point (`interpreter::deliver_breakpoint_if_set`) already reads the frame;
   the object id compares by the export table's identity
   (`debug::object_for_id`), so a moved object still matches.
2. **Answer `canUseInstanceFilters` true** once every event kind JDI allows
   the filter on honours it (JDI allows it on the location, method,
   exception, watchpoint and monitor requests; monitor events are not served,
   `canRequestMonitorEvents` false, so those stay refused by JDI).
3. **Native methods' events** (reported by the native-call funnel at
   location -1) have their receiver in the funnel's arguments; an instance
   native's entry matches on it.

## Cost

None outside a debugging session: the matcher runs only when an event is
already being reported.

## Measure

A JDI probe: two instances of one class run the same method in a loop; a
breakpoint in it with `addInstanceFilter(second)` must stop only for the
second (HotSpot 25). `L1W23JdiConformance` unchanged.
