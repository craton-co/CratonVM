# ZGC round zs, wave s9: review residuals (lane s10/rv7)

> **STATUS (2026-10-19, zt t6/rv3): FIXED IN CODE (items 3 opt-in, 5), PENDING BUILD/PROBE; 1, 2 unchanged.** 3 (`48ca92ab32`, opt-in `CRATONVM_ZGC_OOME_TRACED_POOL_ALL_KINDS=1`): the traced pool also serves `Metaspace` / array-size OOMEs that have their own preallocated default (`CRATONVM_GC_PREALLOCATED_OOME_KINDS`), the slot's `detailMessage` pointing at the default's `String` (no allocation), as `Universe::gen_out_of_memory_error`; test `zt6rv3_the_pool_serves_another_kind_with_its_defaults_message` (the switch-off arm is the kill control). Flip row: a Metaspace-OOME probe (`-XX:MaxMetaspaceSize=16m` class-define loop catching 5 errors, printing identity-distinct count and messages) SAME vs `java -XX:+UseZGC`. 5 (`cbb6471bad`, default ON, `CRATONVM_ZGC_METASPACE_BACKFILL_HOLD_MEMO=0` kill): once a drain's busy-lock retries failed, later drain points keep the hold after one non-blocking try until the class-definition epoch or the collection count moves (stamp on `BackendGcBeans`, per VM); test `zt6rv3_the_hold_stands_only_at_its_own_stamp`; `backfill_retry_passes` counts retry passes for the row (a define-heavy loop with a GC listener: passes ~= holds on the kill arm, ~= stamps moved with the memo).

> **STATUS (2026-10-19, zt t5/pg3): OPEN (items 3 and 5).** 4, 7 FIXED; 6 DECIDED; 1 DECIDED by the local rows (`Zs10Rv7PoolLoader` SAME on all four arms, HotSpot 25.0.3); 2 is the non-ZGC opt-in `CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT` (only if flipped). Open: 3, `hotspot_pool_answer` (vm/src/runtime/exceptions.rs:6772) serves only `Java heap space`; 5, `zgc_metaspace_backfill_for_drain` (vm/src/runtime/interpreter/gc_events.rs:5184) retries at every drain point (t3-jf cross-lane hunk X1: a per-VM memo keyed on the class-store next id / collection count).

> **STATUS (2026-10-19, zt t2/rv): OPEN (items 1-3, 5) -- 4 fixed, 6 decided, 7 fixed.** Checked on `67bc580ab8`. 4: FIXED IN CODE by this lane, PENDING BUILD: `create_vm_oome_object` spends the verdict only for a heap refusal and otherwise DROPS it without spending (vm/src/runtime/exceptions.rs:2935, `ZgcRealHeap::drop_exhausted_verdict` gc/src/zgc.rs:17953, `VmHeap::zgc_drop_exhausted_verdict`; unit test `zt2rv_a_non_heap_oome_drops_the_verdict_without_spending_it`, whose lift arm is the positive control). Opt-in arm only. 7: FIXED (s10/nd9 definer rows). 6: DECIDED (diagnostic counters, not compatibility state; jit/src/tiered.rs is lane fp's). OPEN: 1 (host row RV7-1 only; local rows SAME); 2 (opt-in holder switch; exceptions.rs); 3 (the traced pool still serves only `Java heap space`: `hotspot_pool_answer`, vm/src/runtime/exceptions.rs:6772); 5 (the held keyless record retries at every drain point; vm/src/runtime/interpreter/gc_events.rs, lane jf).

> **STATUS (2026-10-16, zs s10/rv7): OPEN (residuals only; no confirmed soundness defect in
> wave s9).** Filed by the adversarial review of `git diff 2abdab5038..a4cd7c8dbc` minus docs
> (report: `docs/internal/zgc-round-zs-20261014/s10-rv7-report.md`). Every item is low or info
> severity, and each says what would close it. The open items of
> `zgc-zs9rv6-wave-s8-review-residuals-FIXED-20261019.md` are re-graded in the report, §4.

## la: the preallocated traced OOME pool

> **STATUS item 1 (2026-10-16, zs s10/rv7b): NOT A LEAK locally (local runs SAME as HotSpot);
> the host row is RV7-1 below.** Probe `tools/probes/Zs10Rv7PoolLoader.java`.
> - **HotSpot 25.0.3**, `java -XX:+UseZGC -Xmx64m`, 3 runs, identical:
>   `trace traced=true cleared=true` / `drop cleared=true` / `control cleared=false`.
> - **CVM debug build of `93c84d49db`** (Windows), `-XX:+UseZGC -Xmx64m`, one run per arm, the
>   same three lines on all four arms: `--jdk-only` and `--compatible`, each on the default and
>   on `CRATONVM_ZGC_OOME_TRACED_POOL=0`.
> - The `control` line (a loader whose class the probe still holds) proves the
>   `WeakReference` test can fail.
> - The rooted pool error does not keep the loader: the `trace` arm materialises
>   `stackTrace`, whose elements drop `declaringClassObject` in the JDK's `computeFormat`. The
>   result also shows the VM-side trace row keeps nothing; I did not read the code to confirm
>   that.
> - **Caveat:** the probe cannot see from Java that its two errors came from the pool. On the
>   default arm the first two VM-raised `Java heap space` errors take traced slots 0 and 1 by
>   construction (`hotspot_pool_answer`), so they did unless they were raised outside
>   `create_vm_oome_object`.
> - Left open only as the host confirmation; no code change.

1. **A handed-out pool error stays rooted for the life of the VM; HotSpot drops it.**
   `PreallocatedOome::traced` (`vm/src/vm/realms/heap_realm.rs`) is scanned and remapped as a
   root forever, by design ("so their bytes never return to the free list").
   `Universe::gen_out_of_memory_error` instead takes the error out of
   `_preallocated_out_of_memory_error_array`, so once the program drops it, it is garbage.
   - What the root keeps alive: the error's materialised `stackTrace` (a `StackTraceElement[]`;
     the JDK clears each element's `declaringClassObject` after formatting) and the VM-side trace
     row of the error.
   - Suspected, not shown: whether that row holds class metadata live (frame ids of a user
     loader's methods). If it does, an OOME thrown inside a loader that is later dropped (an
     app-server redeploy) keeps the loader's metadata.
   - **Close:** a row with a child loader that throws a heap OOME from its own method, drops
     everything, and asks for a `System.gc()` + `-Xlog:class+unload` (HotSpot unloads it). If it
     shows retention, keep the four objects rooted only until each is handed out (the free-list
     argument needs only the boot allocation to stay put) and let the handed-out error be
     ordinary.
2. **"No heap allocation for the trace" holds only with the registry backtrace.** Under the
   opt-in `CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1`, `capture_throwable_stack_trace` installs a
   `long[]` backtrace holder (`SharedVm::install_backtrace_holder`, `try_alloc_array_full`) on
   the refusing heap: the allocation the pool exists to avoid, in the last hole. It is fallible,
   so a full heap falls back to the registry. HotSpot's preallocated errors use a backtrace
   preallocated with them. **Close:** for a pooled error, use the registry path
   (`store_throwable_backtrace`), or preallocate the holder with the error at boot. Only relevant
   if the holder switch is ever flipped.
3. **The pool serves only `Java heap space`.** HotSpot hands its traced slots to every OOME kind
   (it copies the default's message into the preallocated error). Here a `Metaspace` /
   `GC overhead limit exceeded` OOME takes a traced slot (`take_traced_oome_slot_index`) and is
   then constructed fresh, as before s9. No regression; the identity differs from HotSpot only on
   those kinds. **Close:** set the pool error's `detailMessage` to the default's text for the
   other kinds HotSpot preallocates, and serve them too.

## no2: the exhausted verdict (opt-in)

4. **Any VM-raised OOME on a thread holding a live verdict spends it.** `lift_exhausted_verdict`
   runs in `create_vm_oome_object` for every VM-raised OOME: `throw_runtime_error`,
   `jit_alloc_oom` and the JNI door all come there (its own comment). That includes one unrelated
   to the refusal, such as `Requested array size exceeds VM limit`. `exhausted_spent` is per heap, so a spent mark blocks every thread's verdict until
   a collection leaves the floor free. **Close:** spend only when the OOME being built is
   `Java heap space`, or pass the reason from the ladder that threw.

## ob6: the held keyless cycle record

5. **A held record blocks every notification behind it, and each drain point pays the back-fill
   retries while the class lock stays busy.** `zgc_metaspace_backfill_for_drain` (16
   `yield_now` + `try_read` per drain round) runs again at every drain point while
   `has_pending()` stays true behind the held head record. It is bounded by
   `MAX_SENSOR_ROUNDS` per drain, but not across drain points while one thread holds the class
   lock for write (a long define). No deadlock (the drain never blocks). **Close:** after the
   retries fail once, keep the hold without re-trying until the class-store next id moves or a
   collection ends; or let the delivery thread take the blocking back-fill (it holds no class
   lock).

## rs2: C2 standby (opt-in)

6. **`C2_STANDBY_STARTED` / `C2_STANDBY_PROMOTED` are process-wide** (`jit/src/tiered.rs`): the
   `[jit] c2-standby:` line of one VM counts every VM's workers. These are diagnostic counters,
   not compatibility state, so this is not an AGENTS.md violation. **Close:** move them onto the
   `CompilerCore` if the line is ever read per VM.

## nd8

7. **The `--compatible` definers still have no unit row** (rv6 residual 4; nd9's this wave).
   nd8 added the panic-arm rows for `cl_define_class_basic` and `define_class_via_full` only.
