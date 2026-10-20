// JIT round 21 wave 1, lane indy: a flipped MutableCallSite / VolatileCallSite, by shape.
//
// Target (r20 handoff perf table, R20PIndyMutableSite mcs-flip): 17760 ns per call on w11d, 171 ns
// after CRATONVM_JIT_INDY_DIRECT_RECENT_TARGETS (w17s), against HotSpot 24-30. What a flip does
// here (read of vm/src/runtime/invokedynamic.rs and native-builtins lang_invoke.rs): the JDK's
// MutableCallSite.setTarget stores CallSite.target through MethodHandleNatives.setCallSiteTarget*
// and NOTHING else -- no code invalidation, no recompile, no global lock, no safepoint. Every
// compiled site re-reads the target per call: the optimizing tier's guarded call
// (CRATONVM_JIT_INDY_MUTABLE_GUARD) compares it with the ONE target it planned and calls that
// method on a hit, the bridge otherwise (jit_indy_bridge -> jit_indy_linked_direct_leaf ->
// jit_invoke_dispatch_body, ~300 ns). So after a flip to the target the guard did not plan,
// every call of the batch is a bridge call. HotSpot instead deoptimizes the dependents and
// recompiles them against the new target.
//
// CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE=1 (r21 lane indy, opt-in) recompiles the owner
// method of a retargeted site once the new target has served 1024 bridge calls (at most 1000 per
// site): the "leaf" rows (indy in a small static method called per element) can then run the
// new body for the rest of the batch; the "loop" rows (indy inside the batch loop itself) cannot
// (the running activation keeps the old body; only a bimorphic guard helps them, proposal
// INDY21-1). The "storm" rows flip every 1000 calls, under the threshold: no recompile at all.
//
// The probe generates its call sites with the java.lang.classfile API (JDK 24+), one generated
// class per row so that every row links its own invokedynamic instructions.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21IndyMcsFlip.java):
//   P=tools/probes/out
//   cratonvm -Xmx512m -cp $P R21IndyMcsFlip                                   # default
//   CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE=1 cratonvm -Xmx512m -cp $P R21IndyMcsFlip
//   CRATONVM_JIT_INDY_MUTABLE_GUARD=0 cratonvm -Xmx512m -cp $P R21IndyMcsFlip # control
//   CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC=1 cratonvm -Xmx512m -cp $P R21IndyMcsFlip  # INDY21-1
//   (and both opt-ins together; the bimorphic arm is the one for mcs-flip-loop)
//   cratonvm --compatible -Xmx512m -cp $P R21IndyMcsFlip                      # both modes
//   java -Xmx512m -cp $P R21IndyMcsFlip                                       # HotSpot 25
// Engagement (one run, not for timing): CRATONVM_DBG_JITC=1, grep
//   `[indy-mutable] ... guard cell re-pointed` (a flip back re-uses its cell),
//   `[indy-mutable] ... retarget recompile requested R21IndyGen<k>.call(I)I` (one per flip on the
//   leaf rows, none on the storm rows), `late-bind recompile` / `retry ENQUEUED`, and
//   `[ir] indy MUTABLE guarded call` per optimizing compile of the owner methods.
//   With the bimorphic switch: `[indy-mutable] ... bimorphic, second target cell 0x...` and
//   `(bimorphic: the second target's leaf bridge first)` on the `[ir]` line; mcs-flip-loop and
//   mcs-flip-leaf should then fall toward the stable rows (both targets compiled).
//
// HotSpot expectation: every non-`time` line identical. time rows (ns per call): HotSpot ~1-3 on
// the stable rows, ~1-30 on the flip rows (a recompile per flip, amortised over 50 000 calls).
// Pass for this lane's target: mcs-flip-leaf under the opt-in within ~3x mcs-stable-leaf.

import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.CallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.lang.invoke.VolatileCallSite;
import java.util.Arrays;

public class R21IndyMcsFlip {
    static final int N = 50_000;
    static final int REPS = 9;
    static final int BATCHES = 40;

    static final MethodHandle A;
    static final MethodHandle B;
    static final MethodHandle C;
    static final CallSite[] SITES = new CallSite[16];

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            MethodType t = MethodType.methodType(int.class, int.class);
            A = l.findStatic(R21IndyMcsFlip.class, "addA", t);
            B = l.findStatic(R21IndyMcsFlip.class, "addB", t);
            C = l.findStatic(R21IndyMcsFlip.class, "triple", t);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    public static int addA(int x) { return x + 1; }
    public static int addB(int x) { return x + 2; }
    public static int triple(int x) { return x * 3; }

    /** Bootstrap of every generated instruction: the name is "op<k>", the row's site index. */
    public static CallSite bsm(MethodHandles.Lookup l, String name, MethodType type) {
        return SITES[Integer.parseInt(name.substring(2))];
    }

    /** One generated class: call(I)I = indy; loop(I)J = sum of indy(i); leafLoop(I)J = sum of call(i). */
    static final class Row {
        final MethodHandle call;
        final MethodHandle loop;
        final MethodHandle leafLoop;
        Row(MethodHandle call, MethodHandle loop, MethodHandle leafLoop) {
            this.call = call;
            this.loop = loop;
            this.leafLoop = leafLoop;
        }
    }

    static Row gen(int k, CallSite site) throws Throwable {
        SITES[k] = site;
        ClassDesc gen = ClassDesc.of("R21IndyGen" + k);
        ClassDesc host = ClassDesc.of("R21IndyMcsFlip");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(
                DirectMethodHandleDesc.Kind.STATIC, host, "bsm",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType));
        MethodTypeDesc ii = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
        MethodTypeDesc il = MethodTypeDesc.of(ConstantDescs.CD_long, ConstantDescs.CD_int);
        DynamicCallSiteDesc indy = DynamicCallSiteDesc.of(bsm, "op" + k, ii);
        int st = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        byte[] bytes = ClassFile.of().build(gen, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withMethodBody("call", ii, st, code -> code.iload(0).invokedynamic(indy).ireturn());
            for (int leaf = 0; leaf < 2; leaf++) {
                final boolean viaCall = leaf == 1;
                cb.withMethodBody(viaCall ? "leafLoop" : "loop", il, st, code -> {
                    Label top = code.newLabel();
                    Label end = code.newLabel();
                    code.lconst_0().lstore(1).iconst_0().istore(3);
                    code.labelBinding(top);
                    code.iload(3).iload(0).if_icmpge(end);
                    code.lload(1).iload(3);
                    if (viaCall) {
                        code.invokestatic(gen, "call", ii);
                    } else {
                        code.invokedynamic(indy);
                    }
                    code.i2l().ladd().lstore(1);
                    code.iinc(3, 1).goto_(top);
                    code.labelBinding(end);
                    code.lload(1).lreturn();
                });
            }
        });
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> c = l.defineClass(bytes);
        return new Row(
                l.findStatic(c, "call", MethodType.methodType(int.class, int.class)),
                l.findStatic(c, "loop", MethodType.methodType(long.class, int.class)),
                l.findStatic(c, "leafLoop", MethodType.methodType(long.class, int.class)));
    }

    /** sum over i < n of target(i), for target addA (+1) / addB (+2) / triple. */
    static long expected(MethodHandle target, int n) {
        long base = (long) n * (n - 1) / 2;
        if (target == A) return base + n;
        if (target == B) return base + 2L * n;
        return 3 * base;
    }

    static long mismatches;

    /**
     * ns per call: BATCHES batches of n calls, setTarget(A|B) between batches when flip.
     * leaf: the leafLoop shape (indy in call(I)I, called per element); else the loop shape.
     */
    static double run(Row r, CallSite site, boolean flip, boolean leaf, int n) throws Throwable {
        MethodHandle body = leaf ? r.leafLoop : r.loop;
        long t0 = System.nanoTime();
        for (int b = 0; b < BATCHES; b++) {
            MethodHandle target = flip ? ((b & 1) == 0 ? A : B) : A;
            if (flip) site.setTarget(target);
            long got = (long) body.invokeExact(n);
            if (got != expected(target, n)) mismatches++;
        }
        return (System.nanoTime() - t0) / (double) (BATCHES * (long) n);
    }

    static void row(String name, Row r, CallSite site, boolean flip, boolean leaf, int n) throws Throwable {
        for (int w = 0; w < 3; w++) run(r, site, flip, leaf, n);
        double[] t = new double[REPS];
        for (int i = 0; i < REPS; i++) t[i] = run(r, site, flip, leaf, n);
        Arrays.sort(t);
        System.out.println("time " + name + " " + Math.round(t[REPS / 2]) + " ns/op");
    }

    public static void main(String[] args) throws Throwable {
        // ── facts ──
        MutableCallSite f = new MutableCallSite(A);
        Row fr = gen(0, f);
        System.out.println("fact mcs-a " + (int) fr.call.invokeExact(5));
        f.setTarget(B);
        System.out.println("fact mcs-b " + (int) fr.call.invokeExact(5));
        for (int i = 0; i < 20_000; i++) {
            f.setTarget((i & 1) == 0 ? A : B);
            int got = (int) fr.call.invokeExact(i);
            if (got != i + 1 + (i & 1)) mismatches++;
        }
        System.out.println("fact mcs-every-call-flip-mismatches " + mismatches);
        Thread other = new Thread(() -> f.setTarget(C));
        other.start();
        other.join();
        System.out.println("fact mcs-cross-thread " + (int) fr.call.invokeExact(5));
        try {
            f.setTarget(MethodHandles.constant(long.class, 1L));
            System.out.println("fact mcs-wrong-type no exception");
        } catch (Throwable e) {
            System.out.println("fact mcs-wrong-type " + e.getClass().getName());
        }
        System.out.println("fact mcs-after-wrong-type " + (int) fr.call.invokeExact(5));
        VolatileCallSite v = new VolatileCallSite(A);
        Row vr = gen(1, v);
        System.out.println("fact vcs-a " + (long) vr.loop.invokeExact(10));
        v.setTarget(B);
        System.out.println("fact vcs-b " + (long) vr.loop.invokeExact(10));
        mismatches = 0;

        // ── timed rows ──
        MutableCallSite s2 = new MutableCallSite(A);
        Row r2 = gen(2, s2);
        row("mcs-stable-loop", r2, s2, false, false, N);
        MutableCallSite s3 = new MutableCallSite(A);
        Row r3 = gen(3, s3);
        row("mcs-stable-leaf", r3, s3, false, true, N);
        MutableCallSite s4 = new MutableCallSite(A);
        Row r4 = gen(4, s4);
        row("mcs-flip-loop", r4, s4, true, false, N);
        MutableCallSite s5 = new MutableCallSite(A);
        Row r5 = gen(5, s5);
        row("mcs-flip-leaf", r5, s5, true, true, N);
        MutableCallSite s6 = new MutableCallSite(A);
        Row r6 = gen(6, s6);
        row("mcs-storm-leaf", r6, s6, true, true, 1000);
        VolatileCallSite s7 = new VolatileCallSite(A);
        Row r7 = gen(7, s7);
        row("vcs-flip-leaf", r7, s7, true, true, N);
        System.out.println("fact timed-rows-mismatches " + mismatches);
    }
}
