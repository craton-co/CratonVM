# JIT round 14 wave 2, lane guard: proposals (bimorphic and trap-miss guarded splices)

Status: OPEN (proposal book; ideas, not work items)
Area: `jit/src/ir.rs` (guarded splice builder, trap facts), `jit/src/lib.rs` (IR inline planner), `jit/src/profile.rs` (receiver-profile policies), `jit/src/ir_lower.rs` (guard deopt ids)
Severity: proposals
Found by: round 14 wave 2 lane guard

Context: this wave landed GS-1 (bimorphic guarded splices, default ON,
`CRATONVM_JIT_IR_BIMORPHIC_GUARDED_SPLICE`) and CH5-5 (a trap as the guarded splice's miss edge,
default OFF, `CRATONVM_JIT_IR_GUARDED_SPLICE_TRAP_MISS`); see the landed notes in
`jit-r13-guardsplice-proposals-RETIRED-20260929.md` (GS-1) and `jit-r13-chain5-proposals-RETIRED-20260929.md` (CH5-5). Ranked by
expected benefit over cost.

## G14-1. Forward a field load across a spliced body that cannot write it (HIGH value, medium cost)

**Problem.** CH5-5's fact is on a NODE. `this.index.put(k, v); this.index.get(k)` reads the field
twice; the builder emits two `Load`s (the spliced `put`'s memory state sits between them), so the
second site learns nothing from the first site's trap, and neither does anything else that
re-reads the field (the `hashmap-field` row of
`r13w11-chain5-chain-arm-hashmap-kernel-slower-than-default-20260928.md`). HotSpot's C2 forwards
the second load (memory slices by field) and the class test happens once.

**Proposal.** In the builder, remember `(base node, field index) -> value` for plain
(non-volatile) `getfield`s of the compiling method's own code, invalidated by any `putfield` of
the same field index (any base), any surviving `Op::Call`, `MonitorEnter`/`Exit`, and every
merge. A spliced body's stores are known field by field (`field_info` rows), so a spliced `put`
whose stores are all to `HashMap`/`Node` fields keeps `Holder.index` valid. Then the second
`this.index` IS the first node and the trap fact (and W3-1's exact proof) apply.

**Risk.** medium: JMM (no forwarding across a volatile read/acquire, a monitor, or a call);
the builder must treat a spliced body's `putstatic`/array stores as clobbering nothing of the
field kind only when they provably are other kinds. **First step.** A `CRATONVM_DBG_IR_COMPILES`
census of `getfield`s whose `(base, field)` was loaded earlier in the same merge-free region with
only spliced stores to other fields between them, on CratonBench and the R13 battery.

## G14-2. Keep a trap fact across a loop header the trap dominates (MEDIUM value, low-medium cost)

**Problem.** `trap_exact_facts` is cleared at every merge of the compiling method, loop headers
included, so `m.put(..)` before a loop and `m.get(..)` inside it (the loop-invariant receiver, the
most common repeated use) re-test. The fact is valid at a header whose every forward predecessor
is dominated by the trap and whose back edges come from inside a natural loop.

**Proposal.** At a loop header's activation (`activate_loop_header`), keep the facts when the
header has exactly one forward predecessor (the live fall-through the walk is adding), the header
is not in an irreducible cycle (`ir_optimize::back_edges_dominated_by` logic, or the verifier's
loop analysis), and no OSR entry lands there (an OSR entry is a predecessor the trap does not
dominate). Clear as today everywhere else.

**Risk.** low-medium (the OSR-entry predecessor is the trap-free path to watch). **First step.**
Count, under `CRATONVM_DBG_IR_COMPILES`, the facts cleared at a loop header that met the three
conditions.

## G14-3. One body for two classes that select the same method (MEDIUM value, low cost)

**Problem.** A bimorphic site whose two classes inherit the same method (`P` and `Q` both
extending `Base` without overriding `f`) splices the same bytecode twice behind two tests. Twice
the code and budget for one body.

**Proposal.** When both resolved bodies have the same `(method_key, class_id)`, build
`If(Or(Exact A, Exact B))` (two `ExactClassIs`, one `Or`) and the one-class guarded frame. The
builder's frame then has `receiver_exact_class = 0` (neither class is known on the hit edge);
nested exact sites keep their own tests.

**Risk.** low. **First step.** The GS-1 census line already prints both class ids; add
`same-body` when the planner's two `InlineSite`s agree.

## G14-4. A separate deopt reason for class guards (MEDIUM value, low cost)

**Problem.** Every IR `Op::Guard` is stamped `DeoptReason::UncommonTrap` with
`speculation_id(bci, UncommonTrap)` (`ir_lower`'s `Op::Guard` arm). At one invoke that means the
null-receiver guard (`begin_splice`), the CHA class guard of a synchronized direct call
(`enter_receiver_monitor`) and now the CH5-5 miss trap all share one de-spec key: four null
receivers withdraw the class speculation (and the sync-direct CHA bind, whose planner asks the
same key), and four class misses withdraw a null guard nothing replaced.

**Proposal.** Carry the reason on the node (`Op::Guard { bci, reason }`, or a side table keyed by
guard node id like `Graph::site_trap_pcs`): `ClassCheck` for an `ExactClassIs` condition,
`NullCheck` for a null compare, `UncommonTrap` otherwise; `ir_lower` stamps it and the planners
ask for their own. **Risk.** low; touches every `Op::Guard` producer (shape tables pin the op's
arity: prefer the side table). **First step.** A unit test that the CH5-5 guard's point carries
`ClassCheck`.

## G14-5. A bimorphic site the builder refused should retry as one class (LOW-MEDIUM value, low cost)

**Problem.** A refusal inside either body blames the whole site (one spliced range covers both),
so the W5-2 rebuild drops both bodies and the one-class guard with them.

**Proposal.** Keep a per-compile set "bimorphic refused at pc" beside `ir_splice_skip`; the
rebuild plans that pc as the one-class guarded splice of the class whose body the blamed pc is
NOT in (the sink's pc says which body). **First step.** Count refusals inside bimorphic bodies
under `CRATONVM_DBG_JITC` (`ir-splice-rebuild` lines whose site has a census `planned` line).

## G14-6. Seed the miss call's inline cache with the third class (LOW value, trivial cost; the IC lane's code)

GS-2 (`jit-r13-guardsplice-proposals-RETIRED-20260929.md`) for the bimorphic case: the miss call of a bimorphic
site never sees either profiled class, so its MIC should be seeded with the profile's THIRD class
(or left unseeded), never with a class a test in front of it consumes.

## G14-7. Trap-miss evidence from a trap-free recompile (LOW value, low cost)

CH5-5 reads only the interpreter's receiver profile. After a de-spec the recompile keeps the
call; nothing ever re-tries the trap if the site returns to one class. A per-site counter on the
miss call (GS-5's shape) could re-enable it after a quiet period. **First step.** Only after
CH5-5 is measured default-on worthy.
