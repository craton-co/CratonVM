> **j27 wave 8 (lane W8JNI, 2026-10-28): REVIEWED, PENDING HOST MEASUREMENT.** Re-read both leaves
> (`native-builtins/src/lang_system.rs` `native_system_identity_hash_code_leaf`, `native-builtins/src/lib.rs`
> `native_object_hash_code_leaf`) and `NativeContextImpl::identity_hash_code_installed` (`vm/src/vm/vm_exec.rs`):
> every decline happens before any effect (a heap answer of `0` minted nothing); `System.identityHashCode(null)`
> answers `0` without declining (pure); a null `Object.hashCode` receiver, a reflection type stub and
> `CRATONVM_DBG_VDISP` decline to the full funnel; a non-zero answer is exactly `java_identity_hash`'s (it
> returns a non-zero heap answer unchanged), in both modes. Kinds stated at registration (`Bridge`). No code
> change. What is left is unchanged: item (1) below, the host A/B of `leaf-array-object-hash` /
> `leaf-super-hash` (A = default vs D = `CRATONVM_JIT_IDENTITY_HASH_LEAF=0`, A must not lose, else flip the leaf
> opt-in) with `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS=1` showing `leaf=` near `served` on A.

> **j27 wave 1 (lane JNINAT, 2026-10-28): NOTHING TO RE-LAND -- the leaf IS on dev (base `214b45112c`), PENDING
> HOST MEASUREMENT.** The t1 wave 8 note below ("still ABSENT from dev") is stale: the 2026-10-26 re-land
> `44f0382076` brought commit `6d1c56082c`'s leaf back, and every piece is present on this base:
> `NativeHeapAccess::identity_hash_code_installed` (`native-api/src/registry.rs`, default `None`), its VM
> implementation (`vm/src/vm/vm_exec.rs`, `NativeContextImpl`, now also filing lane hash2's class verdict through
> `note_identity_hash_receiver`), `native_system_identity_hash_code_leaf` / `identity_hash_leaf_enabled`
> (`native-builtins/src/lang_system.rs`), `native_object_hash_code_leaf` and both `with_leaf(true, ..)`
> registrations with `NativeKind::Bridge` stated (`native-builtins/src/lib.rs`), the switch row
> `identity-hash-leaf` (`types/src/flag_groups.rs`, since 2026-10-14), and the lean-funnel healing review fix
> (`vm/src/jit/helpers/dispatch.rs` `try_lean_native_door`, `load_and_forward_validated` under `lean_funnel`).
> Checked by diffing `git show 6d1c56082c` hunk by hunk against the tree (the commit's other items --
> `CRATONVM_MH_IWA_VARARGS_DIRECT`, `CRATONVM_JIT_FFM_DIRECT_DOOR_EXACT_ONLY` -- are not this page's). What is
> left is exactly item (1) of the status below, which only the host can do: `R21HashlockRows` rows
> `leaf-array-object-hash` and `leaf-super-hash`, arm A (default) vs D (`CRATONVM_JIT_IDENTITY_HASH_LEAF=0`), A must
> not lose (else flip the leaf opt-in), plus `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS=1` on A printing `leaf=` near
> `served` for those two sites. Review note (checked, sound): the leaf's `identity_hash_code_installed` now calls
> `note_identity_hash_receiver` (`vm/src/jit/helpers/access.rs`), which returns at once until a compile opened a
> verdict table and, per undecided class, asks `class_hash_code_identity_verdict` through
> `class_manager.try_read_untracked()` -- non-blocking (`None` files nothing and a later call retries), so the
> leaf contract (no blocking, no GC point) holds.

# RD16-3 in round 21: the identity-hash crossings still paid the full native funnel

> **t1 wave 8 status (2026-10-26, lane RL-HASHREF).** Merge `624fae0b49` had dropped the code this page describes.
> Wave 8 re-landed lane hashlock's HASH2-21-1 equals arm as **opt-in** (`CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM=1`,
> never host-measured; tags now `0x5674` / `0x5673` -- `0x5672` became dev's G1 edge-filter tag at merge
> `30a448f9b6` -- table on `ClassRealm`), together with lanes hash / hash2 /
> hashlock (default ON, host-verified in r21). Lane native's leaf (`CRATONVM_JIT_IDENTITY_HASH_LEAF`,
> `identity_hash_code_installed`, `6d1c56082c`) is still ABSENT from dev and was not re-landed here (not this lane's
> list); until it is, item (1) above cannot be measured, and the class-table verdicts are filed at `<clinit>`
> completion and by the compile-time query (plus `NativeContextImpl::identity_hash_code` once its one-line hook in
> `vm_exec.rs` is applied). Probe: `tools/probes/t1/T1W8RlHashrefIdentity.java`.

Status: PENDING HOST MEASUREMENT (JIT round 21 wave 14, lane hashlock, 2026-10-15) -- three things are in code and
wait for the host: (1) the leaf switch's OWN rows, `time leaf-array-object-hash` and `leaf-super-hash` in
`C:\craton\jr21-ssh\probes\R21HashlockRows.java` (arms A = default vs D = `CRATONVM_JIT_IDENTITY_HASH_LEAF=0`; A must
not lose, else flip the leaf opt-in); (2) HASH2-21-1, the compiled `Object.equals(Object)` identity arm, new switch
`CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM` (default ON, `=0` off; section "Round 21 wave 14: the equals identity arm");
(3) HASHLOCK21-1 is DESIGNED here and NOT implemented (section "HASHLOCK21-1 design": the 32-bit mark word has no
free bit for a hash-keeping fast lock, and the steady-state lock of a hashed object is already at HotSpot, 3 vs 2 ns;
what is left is a one-time inflation per object, sized by the new `hash-then-lock-fresh` row). Previous status:
PENDING HOST MEASUREMENT (round-21 host check, 2026-10-15) -- the w1g A/B (w1ab2-w1g.txt, ns, default vs `CRATONVM_JIT_IDENTITY_HASH_LEAF=0`) wins hashmap-identity-get 136 vs 229 and locked-object-hash 160 vs 181 (identity-hash 2 vs 2) but LOSES object-hash 115 vs 96. From w3a on object-hash is answered by the identity arm (3 ns, `jr21-hash-object-hashcode-identity-arm-FIXED-20261014.md`), so the leaf's share of that row is no longer measured. Missing: a leaf A/B on a post-w3a build, the `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS=1` engagement line, and the review fix's `R19Funnel2Crossing` GC battery (none in any round-21 log). Facts SAME 12 (fz21-w3m-R21.txt). Previous status: PENDING HOST MEASUREMENT (code in, JIT round 21, lane native; switch `CRATONVM_JIT_IDENTITY_HASH_LEAF`,
default ON, `=0` off; probe `C:\craton\jr21-ssh\probes\R21NativeIdentityHashLeaf.java`). One review fix in the same
area is FIXED IN CODE (unconditional), PENDING HOST VERIFICATION (section "Review fix").

Re-land (20261026): merge `624fae0b49` had dropped this round-21 code from dev; it is back on dev since `44f0382076` (formatted 3-way re-land, `docs/internal/jit/jr21-merge-624fae0b49-dropped-round-21-code-20261026.md`). The status above describes the code again; its host verification is recorded there.

Area: `native-builtins/src/lib.rs` (`native_object_hash_code_leaf`, the `Object.hashCode` registration),
`native-builtins/src/lang_system.rs` (`native_system_identity_hash_code_leaf`, `identity_hash_leaf_enabled`),
`native-api/src/registry.rs` (`NativeContext::identity_hash_code_installed`), `vm/src/vm/vm_exec.rs` (its VM
implementation), `vm/src/jit/helpers.rs` (`try_lean_native_door`, review fix)
Found by: JIT round 21 lane native (carried item RD16-3, `r20-HANDOFF` item 7, triage r18 section 3.1)

## What was left of RD16-3 on this base

RD16-3 (`docs/internal/jit-proposals/jit-r16-redefine-proposals-RETIRED-20261006.md`) measured compiled
`Object.hashCode`, `System.identityHashCode`, `getClass`, `Class.isArray` and `Thread.holdsLock` at 370-1175 ns.
Since then (`git log -S`, the books under `docs/internal/`):

* r17 w2: the anatomy (`r17-native-funnel-crossing-anatomy-CLOSED-20261006.md`), `CRATONVM_JIT_NATIVE_LEAN_DOOR`
  (ON since r18); the class-manager lock in the bodies (`CRATONVM_CLASS_FACTS_MEMO`, ON).
* SBR5 lane ncall: the per-site lean door at the top of both extern helpers (Route L), `Class.isArray` /
  `isInstance` LEAF.
* r17: `CRATONVM_JIT_OBJECT_GET_CLASS_DIRECT` (`getClass` has a direct helper).
* r19 funnel2/funnel3: walk-once, two-way table, leaf kinds, fresh result, lean funnel on Route L (all opt-in, kept
  OFF).
* nio20 w5 / nio21 w1: an inline header-read PREFIX for `System.identityHashCode` and `Class.hashCode()` in both
  tiers (`try_emit_ir_identity_hash_prefix`, `try_emit_sp_identity_hash_dispatch_prefix`). It answers when the
  header already holds a hash; every decline (first hash, a locked compact instance, a forwarded header) calls the
  native.

What still crossed the full funnel (`safe_native_call_prevalidated_objects`, ~22-25 ns fixed on the host, plus the
JIT-dispatch depth guard and the SATB flush on Route L) on every call: **`Object.hashCode()`** on any receiver whose
class does not override it (no prefix covers a virtual `Object.hashCode` site: the receiver may override), i.e.
every `HashMap` / `HashSet` / `IdentityHashMap`-free identity-keyed map operation, and the prefix's declines of
`System.identityHashCode`. Both natives were registered non-leaf although their common case is a header read.

## Round 21 change: both natives LEAF, declining to the funnel for the monitor case

* `NativeContext::identity_hash_code_installed(obj) -> Option<i32>` (default `None`): the VM answers
  `Some(heap.identity_hash_code(obj))` when that is non-zero -- a header read or the heap's lazy mint-and-CAS, no
  allocation, no lock, no GC point -- and `None` when it is `0` (a thin-locked or inflated object never hashed: the
  full path displaces a hash into the monitor; a `0` from the heap minted nothing). `java_identity_hash` returns a
  non-zero heap answer unchanged, so `Some(h)` is exactly `identity_hash_code`'s answer. The DBG dead-receiver probe
  answers as there.
* `native_system_identity_hash_code_leaf`: `Some(h)` answers; otherwise `decline_leaf_call()` (before any effect)
  and the full funnel runs `native_system_identity_hash_code`. `null` answers `0` on the leaf path (pure).
* `native_object_hash_code_leaf`: a non-null receiver that is not a generic-reflection type stub (class-facts memo,
  the same read `Class.isArray`'s audited leaf makes) and no `CRATONVM_DBG_VDISP` answers from
  `identity_hash_code_installed`; everything else (null receiver -> the NPE, a stub's structural hash, the monitor
  case) declines and the full funnel runs `native_object_hash_code`.
* Registration (`register_essential_natives_with_shims`, both real-JDK modes): under the switch both rows are
  registered inside `with_leaf(true, ..)` with the kind stated (`Bridge`, unchanged). `=0` registers the old
  callbacks non-leaf. The synthetic-JDK `register_synthetic_overrides` rows are unchanged (that mode is not a
  HotSpot target here).

Leaf contract (`NativeMethodRegistry::set_leaf`): no Java allocation, no safepoint / blocking / monitor, no GC
initiation, no JNI pending exception -- on every path that does not decline. The decline path is the established
one (`Class.isAssignableFrom`'s WALK12-3 leaf, `safe_native_call_leaf_impl` re-runs a declined call on the full
funnel). Interpreter, Route L, Route S and the identity-hash prefix's decline all reach the leaf through
`safe_native_call_leaf*`.

Answers are byte-identical to the funnel's in both modes (same function computes them); `--compatible` and
`--jdk-only` register these rows from the same place.

## Review fix (correctness, unconditional): the opt-in lean funnel on Route L healed nothing

`try_lean_native_door` with `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAN_FUNNEL=1` (opt-in, kept OFF in round 19) called
`safe_native_call_jit_lean`, which skips the funnel's argument re-forward on the premise that "the door already
forwarded every argument" -- true for Route S (`forward_jit_reference_args`), false for Route L, which only
validates the raw words (`is_object_address`). A relocation in progress moves an object without moving any epoch
(the FFM door's own note), so the native could be handed a from-space copy. Now the door heals each validated
reference with `load_and_forward_validated` (no walk) when it takes the lean funnel; the leaf and full funnels heal
it themselves as before. No effect with the switch at its default.

## How to verify

`R21NativeIdentityHashLeaf` (single file). Arms: A = default; B = `CRATONVM_JIT_IDENTITY_HASH_LEAF=0`; A `--nojit`;
A `--compatible`; A on G1 and ZGC once each. Every `fact` must equal HotSpot 25's in all arms. Decides:
`time object-hash`, `identity-hash`, `hashmap-identity-get` (A under B); `locked-object-hash` is the decline control
(A must not lose). Engagement: `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS=1` on A prints `leaf=` close to `served` for the
`object-hash` row (the `Object.hashCode` site is served as a leaf), B prints `funnel=` instead.

Tests to re-run: `native-builtins` `ncall_class_leaf_rows`, `math_exact_leaf_rows`, the `vm` lib test
`leaf_native_sites_avoid_invoke_or_native_special_cases`, `intrinsics::object` tests.
Review fix: `R19Funnel2Crossing` (`C:\craton\jr19-ssh\probes\`) under `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAN_FUNNEL=1`
across the GC battery the orchestrator uses for door changes (same answers; a stale argument read as a wrong value
or SIGSEGV).

## Remaining RD16-3 rows

`Thread.holdsLock` (monitor table read; HotSpot 75 ns) and `Class.isArray` (already leaf) are what is left of the
original five; `holdsLock` could be a leaf that declines on an inflated monitor -- NATIVE21-2 in
`jit-r21-native-proposals.md`. A compiled `Object.hashCode` prefix guarded on a non-overriding receiver class
(HotSpot's `hashCode` intrinsic) is NATIVE21-1.

## Round 21 wave 14 (lane hashlock): the leaf switch's own rows

Handoff section 4: `CRATONVM_JIT_IDENTITY_HASH_LEAF` lost `object-hash` 115 vs 96 on w1g, and that row is now
answered inline by the identity arm (lane hash, then lane hash2's class table), so no row measures the leaf any
more. Two receivers still reach the `Object.hashCode` native on every call with the arm ON, and both are answered by
the leaf (the heap holds the hash, no monitor):

* `leaf-array-object-hash`: an `Object`-typed `o.hashCode()` whose receiver is an `int[]`. Both tiers' box-hash
  prefix decline arrays at the kind test (lane hash's HASH21-3), so the call runs `jit_invoke_virtual_mic` -> native
  site door -> `native_object_hash_code_leaf` (aux-word read).
* `leaf-super-hash`: `super.hashCode()` inside an override (`invokespecial java/lang/Object.hashCode`); no prefix
  claims an invokespecial site.

Arms: A = default, D = `CRATONVM_JIT_IDENTITY_HASH_LEAF=0`. A must not lose to D on both rows (D pays the full
funnel). If A loses, the leaf is the cost and the orchestrator flips it opt-in. Engagement:
`CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS=1` on A prints `leaf=` near `served` for the two loop methods'
`hashCode` sites (`leafArrayHashLoop`, and `SuperHash.hashCode`'s invokespecial), D prints `funnel=`. Each row has
its own loop method, so the sites' profiles are their own.

## Round 21 wave 14 (lane hashlock): the equals identity arm (HASH2-21-1)

Switch `CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM` (flag row `object-equals-identity-arm`), default ON, `=0` off.

**What it answers.** At an `invokevirtual java/lang/Object.equals(Ljava/lang/Object;)Z` site (exactly the
`Object`-declared triple: `HashMap.getNode` / `putVal` / `removeNode`, `Objects.equals`, `ArrayList.indexOf`, ...),
a non-null PLAIN receiver (kind test; an array header carries its component's id) of

1. exactly `java/lang/Object` (class id 0),
2. the class the site's inline cache was seeded with, when the VM vouches for it (new query
   `WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TAG`), or
3. any class whose byte in the new per-VM verdict table is PROVEN (`WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TABLE_TAG`;
   the table has the identity-hash table's layout and values, so `ir_lower::identity_hash_class_table_test_bytes`
   tests it unchanged)

answers `this == arg` (`CMP RAX, arg ; SETE AL ; MOVZX EAX, AL`). Everything else declines to the unchanged call.
It sits after the existing `Integer` arm (whose miss now lands here) in both tiers' box-hash prefix
(`ir_lower::Lowerer::try_emit_ir_box_hash_equals_prefix`, `x64::op_invoke::Compiler::try_emit_box_hash_equals_prefix`).
Scratch: IR RAX/RCX/RDX/R10 (the prefix's contract; the argument loads into R10 as the `Integer` tail does),
single-pass RAX/RCX/RDX (argument into RDX, as the `Integer` arm does).

**The verdict** (`vm::jit::helpers::class_equals_identity_verdict`) is lane hash's hash verdict with the `equals`
triple: a concrete, non-array, non-lambda class whose whole superclass chain up to `java/lang/Object` has real bytes
and a fixed hierarchy (`typecheck_miss_is_hierarchy_only`), in which no class DECLARES `equals(Object)Z` or has a
native REGISTERED under that triple. JVMS 5.4.6 then selects `Object.equals` -- a class method on the superclass
chain is found before any interface is searched -- whose bytecode is `this == obj`; both `Object.equals` natives registered in
`native-builtins/src/lib.rs` (`native_object_equals` and the reflection-aware closure) answer reference identity for every such receiver (their structural
arm is for the four reflection type stubs, which are interfaces and refused here). An extra, conservative screen:
every transitive superinterface must be readable and hierarchy-only, carry no native registered under the triple
(several are: `java/security/PublicKey`, the `java/lang/reflect` type interfaces, ...), and declare no NON-abstract
`equals` -- so this VM's by-name dispatch cannot pick an interface's answer either. `None` (class manager busy, or a
class missing from the store) files nothing, so a later ask can still decide it. The verdict is a property of the
class: class ids are never reused in a VM, a redefinition cannot add a method, and a late-loaded overriding subclass
has its own id whose byte is REFUSED or unknown -- so no dependency or deopt is needed (the test is on the EXACT
runtime class).

**Who files it.** (a) The identity-hash leaves (`note_identity_hash_receiver`, the same call that files the hash
verdict): a `HashMap` key is hashed before it is compared, so a key class is seen there first. (b) The compile-time
queries: the equals profiled-class query files what it computes; the hash profiled-class query files the equals
verdict too. (c) The equals table opens together with the identity-hash table (the hash table query opens both
under the switch), because once a class's hash verdict is PROVEN its hashes are answered inline and never reach the
leaf again. Classes that are compared but never hashed are not filed: their sites keep the call (proposal
HASHLOCK21-4).

**Both modes.** The arm answers exactly what the call answers for the same receiver in `--jdk-only` and
`--compatible` (Object.equals is identity in HotSpot, in the bytecode and in both registered natives); classes with
synthetic origin, stubs, VM-internal or proxy classes are refused by `typecheck_miss_is_hierarchy_only`. The
`Integer` arm is unchanged. A ZGC armed read barrier refuses the arm at compile time, like the identity-hash arm (the
pointer compare is the one `if_acmpeq` already makes).

**Tests.** `jit` `x64::op_invoke::jr21_hashlock_sp_equals_arm_tests` (2: the seeded compare, exact-`Object` compare,
table bytes and the answer bytes appear; `=0` emits nothing; a `Number` site emits nothing);
`vm` `jit::helpers::jr21_hashlock_identity_equals_table_tests` (3: one stable table per VM distinct from the hash
table; the hash table query opens the equals table only under the switch; id 0 / sentinel refused, an absent class
stays unknown). `types` flag-surface test (one new row).

**How to verify.** `R21HashlockRows`: arms A = default, C = `CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM=0`, plus
`--nojit`, `--compatible`, ZGC / G1 / generational once. Every `fact` equals HotSpot 25's in every arm, in particular
`cold-equals-table 10101110`, `hot-equals-table 10101110`, `hashmap-collide-answers 5 null 0`,
`late-override-equals 1000 0 1000`, `late-override-mixed 5000`, `*-equals-other false`, `*-equals-null false`.
Decides: `equals-identity-hit`, `equals-identity-miss`, `equals-mixed`, `hashmap-collide-hit`,
`hashmap-collide-miss` (A under C); `equals-override` is the control (A must not lose: one extra compare and a byte
load before its call). Engagement: `CRATONVM_DBG_JITC=1` prints `identity-equals-inline (ir|sp, Object.equals)` with
`class_table=true` on the `equalsLoop` / `equalsMixedLoop` / `HashMap.getNode` sites.

## HASHLOCK21-1 design: lock a hashed compact instance without inflating

Status of the item: DESIGNED, NOT IMPLEMENTED (unsound to build blind, and the remaining lever is small).

**What HotSpot does.** Since JDK 21 lightweight locking (`LM_LIGHTWEIGHT`, the only mode in 25) a fast lock CASes
the mark's lock bits from `01` (unlocked) to `00` (fast-locked) and pushes the object on the owning thread's
lock-stack; the owner is never in the header, so the 31 hash bits stay where they are. Inflation reads the owner
from the lock-stacks; hashing a fast-locked object reads the bits directly.

**What this VM's header leaves.** The mark word is 32 bits: state 2, payload 14, quartet 16 (kind 2, element type 4
+ reserved 2, gc flags 4, gc age 4). A compact instance's hash is 20 bits: the 14 payload bits plus the six
element-type / reserved bits. A thin lock needs its owner slot (11 bits) and recursion (3 bits) in the same 14
payload bits. So a hashed compact word has exactly zero free bits: all 4 state values are taken (NEUTRAL, THIN,
INFLATED, FORWARDED), all 4 flag bits are used (OLD_GEN, MARKED, COMPACT, HEADER), the kind and age bits are the
collectors'. A HotSpot-shaped "fast-locked, owner off-header" state would need one more bit to tell "THIN with
owner in payload" from "THIN with hash in payload".

**Designs considered, and why each is not built:**

1. *Steal an age bit* (max tenuring 7): changes every generational collector's promotion policy for every object,
   to save one inflation per hashed-then-locked object. Not a performance switch any collector would accept.
2. *Displace the low 14 hash bits into the owner's lock record* (THIN word keeps the 6 high bits; a per-slot
   lock-stack `[(object, low bits)]` in `LeaseBlock`'s neighbourhood). Needs: (a) the lock-stack to be a GC root set
   that every collector remaps on a move (ZGC relocation, G1 evacuation incl. self-forward restore, generational
   copy, the sliding compactor); (b) a contender's inflation (`inflate_locked`'s THIN arm) to read the OWNER's
   stack concurrently with the owner's push/pop, i.e. a seqlock on the lessee-only `LeaseBlock` that today is
   deliberately lock-free; (c) a reader that hashes a thin-locked object on another thread to find the bits there
   (today it inflates); (d) `wait`/`notify` unchanged (they inflate first; the displaced bits move into the
   monitor exactly as the NEUTRAL arm does now); (e) both tiers' inline thin-lock fast path to decline a hashed
   word (it already does: `thin_lockable` refuses one), so only the helper path would push. (a) and (b) touch
   every collector and the monitor protocol's single-writer invariant; neither can be verified without builds and
   GC batteries.
3. *Encode "hashed + thin" as an INFLATED word with a reserved owner*: that is inflation by another name; the
   `Monitor` allocation is the cost being removed.

**What is left to win.** Host w3c (`R21HashlockRows`, compiled): `hashed-lock-uncontended` 3 ns vs HotSpot 2,
`hashed-lock-recursive` 15 vs 20 -- the inflated enter/exit of a hashed object is already inline and at HotSpot
(lane hashlock's earlier `CRATONVM_INFLATED_MARK_HASH` removed the hash-read cost: `locked-object-hash` 12 vs 10).
What remains is the one-time inflation per object (one `Monitor` allocation, one index shard insert, pruned when
the object dies) -- the new `hash-then-lock-fresh` row (vs `lock-fresh`) sizes it. Only if that row shows a large
gap AND the census (HASHLOCK21-1's first step: inflations from a NEUTRAL hashed word, under a DBG counter in
`inflate_locked`) shows real programs doing it per object is design 2 worth a dedicated round with the GC owners.

## Review (lane hashlock, wave 14): identity hash vs inflation vs GC move

Checked, sound on this base:

* Hash mint (`ObjectHeader::identity_hash`, NEUTRAL CAS) vs thin lock (`try_thin_lock`, CAS from the same NEUTRAL
  word): one CAS wins; the loser re-reads (hash: THIN -> `Err`, the caller inflates through
  `identity_hash_via_monitor`; lock: hashed word -> inflate, the NEUTRAL arm displaces the hash first).
* Hash mint vs inflation (`inflate_locked` NEUTRAL arm): the inflater builds its monitor from its snapshot; a hash
  CASed in after the snapshot fails the inflater's CAS, which re-snapshots and displaces it.
* `identity_hash_via_monitor` displaces outside the shard lock: a concurrent reader that finds the monitor with no
  displaced hash yet mints and races on `Monitor::displace_hash` (write-once), so both return the winner; the
  winner is then published into the INFLATED word, also write-once (`publish_inflated_hash`).
* Collector flag / age updates are atomic RMWs on the mark word (`fetch_or`/`fetch_and`/`fetch_update`), so they
  cannot drop a concurrently installed hash; every forwarding copy carries the whole header; a G1 self-forward
  restore (`restore_self_forwarded_mark`) puts back the payload and hash-high bits, INFLATED words included.

Filed (fixed, retired 20261026): `docs/internal/gc/jr21-inflate-locked-treats-a-forwarded-mark-as-neutral-20261015.md` --
`inflate_locked`'s catch-all arm also matches a FORWARDED word and would CAS it to INFLATED, destroying the
forwarding pointer, if a mutator ever reached it with a stale reference (an `identity_hash` of a FORWARDED compact
word answers `Err`, and the leaf / funnel then inflates). Not reproduced; no path that hands a mutator a forwarded
object is known on this base.
