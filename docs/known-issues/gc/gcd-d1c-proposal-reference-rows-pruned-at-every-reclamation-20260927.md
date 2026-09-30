# Proposal: prune reference-processor rows at every reclamation site, by registration number, and retire the identity-stamp screen

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 44
> of 54).** Steps 1-2 hold (the concurrent sweep prunes by registration
> number); steps 3 (the identity-stamp screen becomes an audit assertion) and
> 4 (sequence numbers for the other address-keyed tables) not built. **Gate:**
> `CRATONVM_DBG_REFPROC_AUDIT=1` `d_stamp` / `d_shape` refusals at zero on the
> reference probes before step 3 lands. **Size:** S (step 3), M (step 4).

*Filed 2026-09-27 by the GC defects round, wave d1, lane c (`conc`). A
direction, not a defect.*

## Where things stand

A row of the reference processor names its `Reference` (or finalizable) by
ADDRESS. Every consumer that might meet a row whose object died and whose
block was re-issued screens it twice: by shape (a `Reference` subclass, not an
array, at least two fields) and by the identity stamp taken at discovery
(`now == 0 || now == stamp`). The second screen admits any object that has no
identity hash yet, which is most objects, so the pair is a heuristic, and the
pages of the last two rounds keep finding paths where it is the only guard
(`gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`,
the skip-set candidates of the concurrent cycle, the young twin).

This round gave every row a registration number
(`ReferenceEntry::registration_seq`) and used it at ONE reclamation site: the
concurrent sweep drops the rows it freed that were registered before it began
(`remove_reference_objects_registered_before`), by a pure range test, so a
re-issued block can neither keep a dead row nor lose a new one.

## The proposal

Make "a reclamation drops the rows keyed in what it freed" the invariant for
every collector, so no row can ever outlive its object:

1. The stop-the-world paths already know what died (`remove_collected` with
   the collection's own survivor predicate) and run inside the pause, where no
   block can be re-issued before the prune; they need nothing.
2. The other out-of-pause reclamations -- the concurrent sweep (done), the
   old-gen live sweep's in-place frees if one ever runs outside a pause, and
   any future concurrent young reclamation -- report their freed spans and
   call the same entry point with a registration number taken before they
   free.
3. Then the identity-stamp screen becomes an assertion: under
   `CRATONVM_DBG_REFPROC_AUDIT`, a stamp mismatch is a bug report, not a
   silent skip, and the `now == 0` admission can be dropped from the
   skip-set candidates.
4. The same (sequence number, freed spans) pair serves the other
   address-keyed tables that `addr_keyed::drop_address_keyed_rows_in` sweeps
   today by range only: a table that registers a key AFTER the sweep started
   (a new smuggled `long`, a new JNI weak global on a re-issued block) would
   keep it, where a pure range test run after the guard drops would not.
   Today those drops run under the old-gen guard, so the question does not
   arise; a sequence number would let them move out from under it (shorter
   guard holds per slice).

## Cost

One `u64` per row (done). The prune is O(rows) per slice with at least one
freed span; a per-slice address index (the rows sorted by address once per
sweep) would make it O(freed rows) if a census shows large processors.

## How to measure

`concdrv_sweep_reference_rows_dropped` (this round) on the tomcat and
spring-boot drivers says how often the concurrent sweep frees a still-active
`Reference`; `CRATONVM_DBG_REFPROC_AUDIT=1`'s `d_stamp` / `d_shape` refusal
counts before and after say how much the screens were still doing.
