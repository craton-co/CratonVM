# Proposal: one door for every VM-initiated `loadClass`, with the request's kind explicit

**Status: proposal — filed 2026-10-03 by interpreter round i1 wave 39, lane
L5. Not implemented; one piece built in wave 40 (below).**

## Progress (wave 40) — lane L5

* The in-flight rows moved onto the thread, as "The direction" asks:
  `JvmThread::loader_drives_in_flight` replaces the `thread_local!`
  `IN_FLIGHT` of `drive_defining_loader_load_named` (same reads; statics
  ratchet 1408 → 1407). Built as a per-VM-state fix, not as the door.
* Two more rows of the table changed column: the after-global-miss ask in
  `resolve_class_loader_aware` is checked under `--jdk-only`, and the checked
  door now propagates a base-delegation loader's own throwable
  (`i37-L5` Progress (wave 40)). The table above is otherwise current.

## The problem

A VM-initiated `loadClass` has one meaning in the JVMS (§5.3.2: the
initiating loader's answer, or its failure, IS the resolution's outcome),
but CratonVM reaches a user loader through at least eight entry points, each
with its own subset of that meaning, decided by which wrapper the caller
happened to pick:

| Entry point | Throw propagates | `null`/wrong name | Initiating record | Circularity / re-entry |
|---|---|---|---|---|
| `drive_defining_loader_load_checked` (class opcodes, `invokestatic` owner) | yes (`--jdk-only`, bytecode `loadClass`) | NCDFE (wave 39) | yes | CCE / re-ask (waves 38-39) |
| `drive_loader_for_global_name` (JDK-global names; checked since wave 39) | yes | NCDFE | no (name not recordable) | yes |
| `drive_defining_loader_load` (array pre-pass, receiver fallbacks) | no | no answer | no | decline |
| `drive_defining_loader_load_after_global_miss` (stub pre-pass, miss rescue) | no | no answer | no | decline |
| `lambda.rs` `lambda_impl_dispatch_override_driven` | no | no answer | no | decline |
| `vm_exec.rs` `class_via_caller_loader` | no | no answer | no | decline |
| `Class.forName` native (`invoke_load_class_as_the_vm`) | yes (Java-level) | CNFE (wave 39) | yes | loader lock only |
| other natives (`lang_class.rs` descriptor / annotation / nestmate drives) | varies | varies | no | loader lock only |

Every wave since 25 has fixed one row of one column, and each fix had to
trace which wrapper a probe's path took first (the "outer layer answered
first" trap of the common rules). The per-thread in-flight guard is also a
`thread_local!` keyed by a bare `ClassId` (`IN_FLIGHT` in
`drive_defining_loader_load_named`): two VMs driven on one OS thread share it.

## The direction

One function, `request_class_from_loader(shared, thread, request)`, where
`request` names:

* the initiating loader (object and namespace id) and the name;
* the **kind**: `Resolution { entry: (ClassId, cp_index) }` (a JVMS §5.4.3
  resolution: HotSpot's outcome in every column, the entry's record written
  here, not by each opcode), `Reflective` (`Class.forName`: the same, but CNFE
  instead of NCDFE and no entry), or `Lookup` (CratonVM's own questions — a
  stub-avoidance probe, a lambda dispatch override — which never record,
  never propagate, and are counted so they can be retired one by one);
* the policy bits the mode decides (`--jdk-only` / `--compatible`), read once
  from the VM config instead of at each wrapper.

The in-flight rows move onto `JvmThread` (per VM by construction; the
statics ratchet counts the `thread_local!` today), carrying the nesting
count wave 39 uses for the parallel-capable re-ask.

## What it would buy

* A new JVMS rule (the itable constraint check, a new census column) lands
  in one place and reaches every resolution site.
* The `--compatible` decision of `i37-L5` / `i39-L5-compatible-mode-…`
  becomes one policy switch instead of a hunt through eight wrappers.
* The `Lookup` count names the CratonVM-only drives that remain, which is
  the retirement list for the JDK-only programme.

## Cost and risk

Cold path only (a `loadClass` upcall dominates any dispatch on the request
kind). The risk is behavioural: each `Lookup` caller today silently falls
back; moving one to `Resolution` is a `--jdk-only` behaviour change that
needs the `CRATONVM_DBG=access` census on the suite, Spring Boot and Tomcat,
as waves 37-39 did for their rows.
