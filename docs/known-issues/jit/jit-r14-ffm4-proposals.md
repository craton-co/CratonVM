# JIT round 14 wave 4, lane ffm4: proposals (ranked)

Status: OPEN (proposal book; ideas, not work items, until the owner queues one)
Area: FFM (`native-builtins/src/panama*.rs`, `phases_late/foreign_ffm.rs`)
Found by: round 14 wave 4 lane ffm4

## FFM4-1: one `Cleaner` registration per implicit session (FFM7-2 on top of FFM3W-1's counter)

- What: FFM3W-1 (this wave) wraps EVERY Java close action of an `Arena.ofAuto()` session in its
  own `CratonVM$FfmAutoSessionAction` and registers each with the `Cleaner` (one
  `PhantomCleanableRef` per action), counting them on the session's block row. HotSpot registers
  ONE `ResourceList` per implicit session. Keep one wrapper per session instead (its reference
  in the session's action list next to the `Cleaner` marker, e.g. the marker replaced by the
  wrapper), append later actions to the wrapper's list (CAS on a node chain, as
  `p67_session_push_action_cas` does), and let its single `run()` run them LIFO -- HotSpot's
  order -- and free the row. `pending` then becomes 0/1.
- Benefit: medium for jextract-style code that registers `reinterpret(.., auto, cleanup)` per
  struct (N phantom refs and N cleaner wake-ups become 1); exact HotSpot action order.
- Cost: medium (`p67_implicit_session_cleaner`'s "only entry is the marker" test must learn the
  wrapper; the append must be atomic against concurrent adders on a shared auto arena).
- Risk: medium (an action appended after the cleaner started running the list must still run:
  the append has to detect a closed list and fall back to a separate registration).
- First step: count `Cleaner.register` calls per implicit session in `R14Ffm4AutoActionFree`
  (a `CRATONVM_DBG_...` census line) to size the win on a jextract sample.

## FFM4-2: decode a `List12` member list (the real JDK's one- and two-member groups)

- What: `foreign_ffm::p67_group_members` decodes an array, an `ArrayList`, a `ListN` and (this
  wave) the `cratonvm/internal/UnmodifiableList` wrapper, but not `ImmutableCollections$List12`
  (`e0`, `e1`, no backing array). The real `AbstractGroupLayout` constructor stores
  `List.copyOf(elements)`, and JDK 25 `listCopy` turns a two-element null-allowing `ListN`
  (`Stream.toList()`, what `MemoryLayout.structLayout` passes) into a `List12`. So any group the
  JDK's own bytecode built with one or two members (`--jdk-only`'s `structLayout(I, I)`) reads as
  member-less wherever it reaches this VM's readers: the link checks pass silently and
  `layout_struct_to_ffi_type` answers "StructLayout has no members". Change the reader to return
  a small `Vec<ObjectRef>` (or a `(Option<array>, [ObjectRef; 2])` shape) so `List12` needs no
  allocation, and treat `e1 == ImmutableCollections.EMPTY` as absent.
- Benefit: medium if any `--jdk-only` path hands a JDK-built group to these readers; zero if every
  such path runs the JDK's own linker. Unknown by reading.
- Cost: small-medium (11 callers of `p67_group_members`).
- Risk: low.
- First step: a `--jdk-only` probe passing `structLayout(JAVA_INT, JAVA_INT)` by value to a
  downcall and an upcall with `CRATONVM_DBG_ATHROW=1`, to see whether `panama_libffi` is reached.

## FFM4-3: retire `CRATONVM_FFM_GROUP_MEMBERS_UNWRAP` once `List.copyOf` identity has soaked

- What: lane compat4 made `--compatible`'s `List.copyOf` return a real immutable list unchanged
  (`CRATONVM_COMPAT_COPYOF_REAL_IMMUTABLE_IDENTITY`), which removes the wrapper this wave's FFM
  unwrap exists for, for that shape. The unwrap still covers a wrapper built from a NON-immutable
  source (a program-built `ArrayList` passed to a JDK constructor that copies it). Keep both for a
  round; then decide with a census of how often the unwrap fires (a counter printed under
  `CRATONVM_DBG_...`, which needs a production reader per the orphan-instrument gate).
- Benefit: low (one switch fewer). Cost: small. Risk: low.
- First step: the counter.

## FFM4-4: regenerate the jdk-only dead/no-image baselines after row deletions

- What: rounds 14 waves 2 and 4 hand-deleted field-shaped registrations and amended only the
  kind map; `scripts/baselines/jdk-only-dead-everywhere*.tsv` and
  `jdk-only-no-image-methods.tsv` still list the deleted rows (with stale line numbers). Rerun
  `scripts/jdk-only-no-image-methods.py` / the dead-everywhere sweep on the Linux host and commit.
- Benefit: low (baseline honesty). Cost: small (host run). Risk: none.
- First step: run the two scripts at this wave's tip.

## FFM4-5: `withByteAlignment` below a group's minimum alignment

- What: `p67_layout_with_byte_alignment` copies any receiver; for a group JDK 25
  (`AbstractGroupLayout.withByteAlignment`) refuses `byteAlignment < minByteAlignment` with
  `IllegalArgumentException: Invalid alignment constraint`. Reachable only where the native row
  wins for a group receiver (synthetic-JDK carriers typed as the interface). Read
  `minByteAlignment` (by name) and refuse.
- Benefit: low. Cost: small. Risk: low.
- First step: `R14Ffm4OverAlignedGroup` plus `structLayout(I, I).withByteAlignment(2)` under
  `--synthetic-jdk`.
