// Groundwork for a Java-level witness of an IR SITE TRAP taken at runtime.
//
// READ THIS FIRST: as of 2026-09-07 this probe does NOT reproduce a taken site
// trap, and the four things that stopped it are the value of the file. The
// defect it was written for was reproduced instead by a Rust test against an
// in-repo fixture (`vm/tests/jit_deopt_sink_resumes_a_side_effecting_trap.rs`),
// which reaches `execute`'s tier-up sink directly. This probe stays because the
// remaining open hazard --
// `docs/known-issues/jit/jit-bridge-sinks-re-run-a-side-effecting-body-20260907.md`
// -- needs a JAVA-level witness that enters through ordinary bytecode dispatch,
// which is a path no Rust `vm.invoke` can take, and this is how far that got.
//
// ## The shape being aimed at
//
// Straight off the 2026-09-07 crash population (H2 `FreeSpaceList$BlockRange
// .toString()`, Spring `CorsConfiguration.addAllowedOrigin`,
// hibernate-reactive `ReactiveEntityInitializerImpl
// .reactiveInitializeEntityInstance`):
//
//   1. the callee carries an `invokedynamic` -- `makeConcatWithConstants`, what
//      every `String +` over a non-constant compiles to -- which the optimizing
//      IR tier cannot lower, so it PLANTS AN UNCONDITIONAL UNCOMMON TRAP at
//      that bci and compiles the rest (`CRATONVM_JIT_IR_SITE_TRAP`, default ON);
//   2. the same body commits a side effect before that bci, so
//      `bytecode_commits_side_effect` is true and a whole-method replay from
//      entry is NOT observably equivalent;
//   3. the callee is hot enough to be at the OPTIMIZING tier when the trap
//      fires.
//
// ## The four walls, in the order they were hit
//
// Each was found by running this file and reading `CRATONVM_DBG=jitc` plus
// `CRATONVM_DBG_IR_STAGE=1`. All four are properties of the compiler, not of
// the workload, so they will stop the next attempt too.
//
//   1. `hot` never leaves C1. The tiered manager's C2 door wants
//      `invocation_count >= c2_threshold` (20 000), and the interpreter's
//      invocation counter stops advancing once the method is compiled at C1 --
//      so raising the iteration count does nothing. Take the
//      `Interpreter -> C2` door instead, by putting C1 out of reach:
//
//        CRATONVM_TIER_C1_THRESHOLD=100000
//        CRATONVM_TIER_C2_THRESHOLD=600
//        CRATONVM_TIER_C2_MIN_INVOCATIONS=500
//
//      Reported as `[ir] admission ...: admitted to the optimizing pipeline`
//      instead of `optimize=false -- the C1/fast tier was requested, not C2`.
//
//   2. `new` refuses the whole body. An allocation-bearing method does not
//      reach the optimizing tier without `CRATONVM_JIT_C2_ALLOC_UPGRADE`, so
//      the `new StringBuilder()` this file started with made every other
//      arrangement moot. There is no `new` here now.
//
//   3. `putstatic` refuses the whole body: `[ir] IrBuilder::build has no
//      lowering for opcode 0xb3`. The obvious side effect -- bumping a static
//      counter -- is the one the front end cannot lower. Hence the array store
//      below, which is `opcode_commits_side_effect` just the same.
//
//   4. **Still open.** With all of the above, the build reaches the
//      `invokedynamic` arm and bails there:
//      `[ir] IrBuilder::build refused at ir.rs:7586`, the
//      `indy_trap_sites.get(&pc)` miss. The trap is planted only when the
//      compile door supplied a `cp_invokedynamic_descriptor_resolver` and it
//      resolved EVERY indy site in the method ("all or nothing", `lib.rs`);
//      the door this path takes supplies none, so the method keeps the
//      single-pass backend and no trap exists to take. Field runs DO plant them
//      (H2 reported `invokedynamic=21` planted, 1 taken), so the next step is
//      to find which door passes the resolver and enter through that one --
//      not to change this Java.
//
// Correctness, not throughput: `SINK[0]` must equal ITERS exactly. A
// whole-method replay would count some iterations twice, which is the
// observable this file exists to provide once a trap is actually taken.
public class IndySiteTrapSinkProbe {

    /** The side effect. An ARRAY STORE and not a `putstatic`: see wall 3. */
    static final int[] SINK = new int[1];

    /** Side effect BEFORE the `invokedynamic` concat. No `new`: see wall 2. */
    static String hot(int i, String s) {
        SINK[0] = SINK[0] + 1;           // iastore -> the side effect
        return s + "-" + i;              // invokedynamic makeConcatWithConstants
    }

    public static void main(String[] args) {
        final int ITERS = Integer.getInteger("probe.iters", 200000);
        final String base = "x";
        long acc = 0;
        for (int i = 0; i < ITERS; i++) {
            acc += hot(i, base).length();
        }
        System.out.println("sink=" + SINK[0] + " expected=" + ITERS
                + " acc=" + acc);
        if (SINK[0] != ITERS) {
            throw new AssertionError("the side effect ran " + SINK[0]
                    + " times for " + ITERS + " calls");
        }
        System.out.println("OK");
    }
}
