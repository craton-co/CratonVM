// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 5 (lane OSRASM): the three OSR shapes javac cannot produce, as a
// HAND-ASSEMBLED class. Page:
// docs/feature-designs/jit-osr-exit-and-recompile.md ("Uncovered",
// and the OSRASM row).
//
// `J27OsrAsm` is built below as a byte array and defined through a private
// ClassLoader. It is a version 49.0 class: HotSpot (and CratonVM,
// `classloading/src/bytecode_verifier.rs`) verify it with the old
// type-inferencing verifier, so its branches need no StackMapTable. (52 would
// need correct frames; 50 would try the split verifier first and fail over.)
//
//   refOnStack(I)I     an `int[1]` that lives ONLY on the operand stack across
//                      the back edge (stack [arr] at the loop header pc 5);
//                      each iteration does arr[0] += i and allocates a
//                      garbage int[16] (300,000 of them: several young GCs
//                      while the array is a stack-only root). Returns
//                      arr.length + arr[0].
//   handlerHeader(I)I  a loop header (pc 14) that is ALSO the handler of two
//                      protected `athrow`s: first entered by a throw at pc 13,
//                      then by the back edge at pc 38 (stack [exc]) and, every
//                      1000th iteration, by `dup; athrow` at pc 35. Body:
//                      acc = (acc ^ i) * 31 + i; i++.
//   mixedStack(I)J     a back-edge target (pc 5) with stack [int a, long b]:
//                      b = (b + i) * 3; a ^= (int) b, with dup2_x1 / dup_x2 /
//                      pop shuffling under the long. Returns b ^ a.
// All three loops are do-while (the body runs once for n = 0).
//
// Each method runs once COLD with n = 300,000 (about 300,000 back edges in one
// activation, so every OSR door is asked), then 3,000 times with n = k % 257
// (the warm arm: method-entry compiles of the same shapes), then once more
// with n = 300,000. The facts are the computed values only; whether a JVM
// enters an OSR body cannot change them.
//
// Run:
//   javac -d <dir> J27OsrAsmRows.java   (any --release >= 8; no jasm needed)
//   java -cp <dir> J27OsrAsmRows > hs.txt
//   cratonvm --cp <dir> J27OsrAsmRows | diff - hs.txt
// in --real-jdk, --compatible, --nojit, and once each with
// CRATONVM_BG_COMPILE=0 (inline OSR), CRATONVM_JIT_OSR_OPTIMIZING=0
// (single-pass only) and CRATONVM_JIT_NO_OSR_EMPTY_STACK_ENTRY=1 (the
// emitter's empty-stack rule off: the door's own stack check must still
// refuse). -Dj27.osrasm.timing=true adds `time` lines (not facts).
//
// Expected HotSpot 25 output (worked out by simulating the bytecode with
// 32/64-bit wrap; not yet confirmed on a HotSpot run -- confirm first; HotSpot's
// OSR migration buffer carries locals and monitors only, and C2 bails such
// a compile with "OSR starts with non-empty stack", so whether HotSpot
// enters an OSR body or not the values are the interpreter's):
//   fact osrasm-define J27OsrAsm 49
//   fact osrasm-ref-on-stack-cold 2050177041
//   fact osrasm-ref-on-stack-warm 31608806
//   fact osrasm-ref-on-stack-big 2050177041
//   fact osrasm-handler-header-cold -399149568
//   fact osrasm-handler-header-warm -570369984
//   fact osrasm-handler-header-big -399149568
//   fact osrasm-mixed-stack-cold -1305432212879056442
//   fact osrasm-mixed-stack-warm -5991886265594514701
//   fact osrasm-mixed-stack-big -1305432212879056442
// (Small-n spot checks of the same simulation, by hand: refOnStack 0..3 ->
// 1 1 2 4; handlerHeader 0..3 -> 0 0 32 1056, 1000 and 1001 -> -708281728;
// mixedStack 0..2 -> 7 7 4.)
//
// What CratonVM should do, per shape (read 2026-10-28, OSRASM; no build):
//   All three: the loop-header pc has a non-empty operand stack on EVERY
//   arrival (verified bytecode fixes the stack depth per pc), so no entry is
//   ever taken; the facts are the interpreter's. The cost of asking is bounded:
//   * default (CRATONVM_BG_COMPILE on): `try_osr_offer` (vm/src/runtime/
//     interpreter.rs) does not look at the stack. The first due offer (1000
//     back edges) requests a background OSR task and resets the count
//     uncharged. The worker's optimizing route refuses the bci
//     (`emit_osr_entry_stubs`: "operand stack not empty at this bci"; for
//     handlerHeader possibly "a handler reaches this bci ..." first); the
//     single-pass compile writes -1 at that pc (`bytecode_walk.rs`,
//     operand-stack-live, counted in OSR_EMPTY_STACK_REFUSALS) and publishes
//     the body, `compile_osr_artifact` memos `mark_osr_entry_rejected` and
//     returns None (one `osr_fail_count` charge, not a denial). The next due
//     offer finds the published body unenterable here
//     (`published_but_unenterable`: osr_published_but_unenterable) and retires
//     the header for the activation with OSR_MAX_ATTEMPTS, charging one
//     refused activation to the method-wide budget. If the single-pass
//     compile instead bails outright, the request is retried at most
//     MAX_TIER_FAIL_RETRIES (3) times, then `Refused` retires the header.
//     So: one (at most three) wasted OSR compile per method, then a poll every
//     OSR_POLL_STRIDE (256) back edges that `should_try_osr` answers "no".
//     (Proposal: docs/feature-designs/jit-osr-exit-and-recompile.md.)
//     j27 wave 5 (lane OSRRETIRE) implements it: since then, both compile
//     modes retire the header on its FIRST due offer, before any request
//     (`vm/src/runtime/interpreter/osr_live_stack.rs`,
//     CRATONVM_JIT_OSR_LIVE_STACK_RETIRE, default ON, =0 restores the above).
//     Engagement check (facts must not change):
//       CRATONVM_DBG_JITC=1 cratonvm --cp <dir> J27OsrAsmRows 2>err.txt
//       grep 'osr live-stack retire J27OsrAsm\.' err.txt   -> at least one line
//         (the cold activation; the `-big` one too if it still interprets), for refOnStack (depth=1),
//         handlerHeader (depth=1) and mixedStack (depth=2: ValueStack keeps
//         a long in one slot)
//       grep 'osr bg request J27OsrAsm\.' err.txt          -> nothing
//     With CRATONVM_JIT_OSR_LIVE_STACK_RETIRE=0 the `osr bg request` lines
//     come back and the retire lines go.
//   * CRATONVM_BG_COMPILE=0: `try_osr` (jit_bridge.rs) refuses at
//     `!frame.stack.is_empty()` before any compile; five charged offers (at
//     1000, 2000, 4000, 8000, 16000 back edges), then retired. No compile.
//   * a body entered anyway (only if both the emitter rule is off AND the door
//     check were removed): `validate_osr_entry` refuses OSR_REFUSE_OPERAND_STACK
//     ("the OSR trampoline seeds locals only").
//   handlerHeader: entering the handler through a throw never asks the OSR
//   door (only back edges do), and since no body is entered there is no
//   OSR exit / re-entry memo traffic. The warm arm is where the compilers
//   meet these shapes for real: the IR builder phis operand-stack slots at a
//   loop header (`stack_phis`) and the single-pass compiler must carry the
//   stack across the merge; a red `-warm` or `-big` row with a green `-cold`
//   row is a whole-method miscompile, not an OSR defect.
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class J27OsrAsmRows {
    static final int BIG = 300_000;
    static final int WARM = 3_000;
    static final boolean TIMING = Boolean.getBoolean("j27.osrasm.timing");

    static final Class<?> ASM;
    static final MethodHandle REF_ON_STACK;
    static final MethodHandle HANDLER_HEADER;
    static final MethodHandle MIXED_STACK;
    static {
        try {
            byte[] bytes = asmClass();
            ASM = new ClassLoader(J27OsrAsmRows.class.getClassLoader()) {
                Class<?> define() {
                    return defineClass("J27OsrAsm", bytes, 0, bytes.length);
                }
            }.define();
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodType ii = MethodType.methodType(int.class, int.class);
            REF_ON_STACK = lookup.findStatic(ASM, "refOnStack", ii);
            HANDLER_HEADER = lookup.findStatic(ASM, "handlerHeader", ii);
            MIXED_STACK = lookup.findStatic(ASM, "mixedStack",
                    MethodType.methodType(long.class, int.class));
        } catch (ReflectiveOperationException | IOException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // ---- the class file -------------------------------------------------

    /** Constant pool indices used by the method bodies. */
    static final int CP_RUNTIME_EXCEPTION = 6;
    static final int CP_RTE_INIT = 10;
    static final int CP_CODE = 16;

    static byte[] asmClass() throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream o = new DataOutputStream(bos);
        o.writeInt(0xCAFEBABE);
        o.writeShort(0);
        o.writeShort(49);
        o.writeShort(17);
        o.writeByte(1); o.writeUTF("J27OsrAsm");                   // #1
        o.writeByte(7); o.writeShort(1);                           // #2
        o.writeByte(1); o.writeUTF("java/lang/Object");            // #3
        o.writeByte(7); o.writeShort(3);                           // #4
        o.writeByte(1); o.writeUTF("java/lang/RuntimeException");  // #5
        o.writeByte(7); o.writeShort(5);                           // #6
        o.writeByte(1); o.writeUTF("<init>");                      // #7
        o.writeByte(1); o.writeUTF("()V");                         // #8
        o.writeByte(12); o.writeShort(7); o.writeShort(8);         // #9
        o.writeByte(10); o.writeShort(6); o.writeShort(9);         // #10
        o.writeByte(1); o.writeUTF("refOnStack");                  // #11
        o.writeByte(1); o.writeUTF("(I)I");                        // #12
        o.writeByte(1); o.writeUTF("handlerHeader");               // #13
        o.writeByte(1); o.writeUTF("mixedStack");                  // #14
        o.writeByte(1); o.writeUTF("(I)J");                        // #15
        o.writeByte(1); o.writeUTF("Code");                        // #16
        o.writeShort(0x0021);   // public super
        o.writeShort(2);        // this
        o.writeShort(4);        // super
        o.writeShort(0);        // interfaces
        o.writeShort(0);        // fields
        o.writeShort(3);        // methods
        method(o, 11, 12, 5, 2, refOnStackCode(), new int[0][]);
        method(o, 13, 12, 3, 4, handlerHeaderCode(), new int[][] {
                {13, 14, 14, CP_RUNTIME_EXCEPTION},
                {35, 36, 14, CP_RUNTIME_EXCEPTION}});
        method(o, 14, 15, 5, 2, mixedStackCode(), new int[0][]);
        o.writeShort(0);        // class attributes
        o.flush();
        return bos.toByteArray();
    }

    static void method(DataOutputStream o, int name, int desc, int maxStack, int maxLocals,
            byte[] code, int[][] handlers) throws IOException {
        o.writeShort(0x0009);   // public static
        o.writeShort(name);
        o.writeShort(desc);
        o.writeShort(1);
        o.writeShort(CP_CODE);
        o.writeInt(2 + 2 + 4 + code.length + 2 + 8 * handlers.length + 2);
        o.writeShort(maxStack);
        o.writeShort(maxLocals);
        o.writeInt(code.length);
        o.write(code);
        o.writeShort(handlers.length);
        for (int[] h : handlers) {
            o.writeShort(h[0]);
            o.writeShort(h[1]);
            o.writeShort(h[2]);
            o.writeShort(h[3]);
        }
        o.writeShort(0);        // Code attributes
    }

    static byte[] bytes(int... b) {
        byte[] out = new byte[b.length];
        for (int i = 0; i < b.length; i++) {
            out[i] = (byte) b[i];
        }
        return out;
    }

    /** locals: 0 n, 1 i. Header pc 5, stack [int[]] there. */
    static byte[] refOnStackCode() {
        return bytes(
                0x04,             //  0 iconst_1
                0xbc, 0x0a,       //  1 newarray int
                0x03,             //  3 iconst_0
                0x3c,             //  4 istore_1
                0x59,             //  5 dup                  [arr arr]
                0x03,             //  6 iconst_0             [arr arr 0]
                0x5c,             //  7 dup2                 [arr arr 0 arr 0]
                0x2e,             //  8 iaload               [arr arr 0 v]
                0x1b,             //  9 iload_1
                0x60,             // 10 iadd                 [arr arr 0 v+i]
                0x4f,             // 11 iastore              [arr]
                0x10, 0x10,       // 12 bipush 16
                0xbc, 0x0a,       // 14 newarray int         [arr tmp]
                0x57,             // 16 pop                  [arr]
                0x84, 0x01, 0x01, // 17 iinc 1 1
                0x1b,             // 20 iload_1
                0x1a,             // 21 iload_0
                0xa1, 0xff, 0xef, // 22 if_icmplt 5 (-17)    [arr] across the back edge
                0x59,             // 25 dup
                0xbe,             // 26 arraylength          [arr len]
                0x5f,             // 27 swap                 [len arr]
                0x03,             // 28 iconst_0
                0x2e,             // 29 iaload               [len v]
                0x60,             // 30 iadd
                0xac);            // 31 ireturn
    }

    /** locals: 0 n, 1 i, 2 acc, 3 exc. Header pc 14 = handler of [13,14) and [35,36). */
    static byte[] handlerHeaderCode() {
        return bytes(
                0x03,             //  0 iconst_0
                0x3c,             //  1 istore_1
                0x03,             //  2 iconst_0
                0x3d,             //  3 istore_2
                0xbb, 0x00, CP_RUNTIME_EXCEPTION, //  4 new RuntimeException
                0x59,             //  7 dup
                0xb7, 0x00, CP_RTE_INIT,          //  8 invokespecial <init>()V
                0x4e,             // 11 astore_3
                0x2d,             // 12 aload_3
                0xbf,             // 13 athrow               -> 14
                0x1c,             // 14 iload_2              [exc] (handler AND loop header)
                0x1b,             // 15 iload_1
                0x82,             // 16 ixor
                0x10, 0x1f,       // 17 bipush 31
                0x68,             // 19 imul
                0x1b,             // 20 iload_1
                0x60,             // 21 iadd
                0x3d,             // 22 istore_2
                0x84, 0x01, 0x01, // 23 iinc 1 1
                0x1b,             // 26 iload_1
                0x11, 0x03, 0xe8, // 27 sipush 1000
                0x70,             // 30 irem
                0x9a, 0x00, 0x05, // 31 ifne 36
                0x59,             // 34 dup                  [exc exc]
                0xbf,             // 35 athrow               -> 14
                0x1b,             // 36 iload_1
                0x1a,             // 37 iload_0
                0xa1, 0xff, 0xe8, // 38 if_icmplt 14 (-24)   [exc] across the back edge
                0x57,             // 41 pop
                0x1c,             // 42 iload_2
                0xac);            // 43 ireturn
    }

    /** locals: 0 n, 1 i. Header pc 5, stack [int a, long b] there. */
    static byte[] mixedStackCode() {
        return bytes(
                0x03,             //  0 iconst_0
                0x3c,             //  1 istore_1
                0x10, 0x07,       //  2 bipush 7             [a]
                0x0a,             //  4 lconst_1             [a b]
                0x1b,             //  5 iload_1
                0x85,             //  6 i2l
                0x61,             //  7 ladd                 [a b+i]
                0x06,             //  8 iconst_3
                0x85,             //  9 i2l
                0x69,             // 10 lmul                 [a b']
                0x5d,             // 11 dup2_x1              [b' a b']
                0x88,             // 12 l2i                  [b' a (int)b']
                0x82,             // 13 ixor                 [b' a']
                0x5b,             // 14 dup_x2               [a' b' a']
                0x57,             // 15 pop                  [a' b']
                0x84, 0x01, 0x01, // 16 iinc 1 1
                0x1b,             // 19 iload_1
                0x1a,             // 20 iload_0
                0xa1, 0xff, 0xf0, // 21 if_icmplt 5 (-16)    [a b] across the back edge
                0x5d,             // 24 dup2_x1              [b a b]
                0x58,             // 25 pop2                 [b a]
                0x85,             // 26 i2l
                0x83,             // 27 lxor
                0xad);            // 28 lreturn
    }

    // ---- the runs -------------------------------------------------------

    static int callInt(MethodHandle mh, int n) {
        try {
            return (int) mh.invokeExact(n);
        } catch (Throwable t) {
            throw new AssertionError(t);
        }
    }

    static long callLong(MethodHandle mh, int n) {
        try {
            return (long) mh.invokeExact(n);
        } catch (Throwable t) {
            throw new AssertionError(t);
        }
    }

    static void intShape(String name, MethodHandle mh) {
        long t0 = System.nanoTime();
        fact(name + "-cold", callInt(mh, BIG));
        long t1 = System.nanoTime();
        int sum = 0;
        for (int k = 0; k < WARM; k++) {
            sum += callInt(mh, k % 257);
        }
        fact(name + "-warm", sum);
        long t2 = System.nanoTime();
        fact(name + "-big", callInt(mh, BIG));
        timing(name, t0, t1, t2, System.nanoTime());
    }

    static void longShape(String name, MethodHandle mh) {
        long t0 = System.nanoTime();
        fact(name + "-cold", callLong(mh, BIG));
        long t1 = System.nanoTime();
        long sum = 0;
        for (int k = 0; k < WARM; k++) {
            sum += callLong(mh, k % 257);
        }
        fact(name + "-warm", sum);
        long t2 = System.nanoTime();
        fact(name + "-big", callLong(mh, BIG));
        timing(name, t0, t1, t2, System.nanoTime());
    }

    static void timing(String name, long t0, long t1, long t2, long t3) {
        if (TIMING) {
            System.out.println("time " + name + " cold-us=" + (t1 - t0) / 1000
                    + " warm-us=" + (t2 - t1) / 1000 + " big-us=" + (t3 - t2) / 1000);
        }
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    public static void main(String[] args) {
        fact("osrasm-define", ASM.getName() + " " + asmMajor());
        intShape("osrasm-ref-on-stack", REF_ON_STACK);
        intShape("osrasm-handler-header", HANDLER_HEADER);
        longShape("osrasm-mixed-stack", MIXED_STACK);
    }

    static int asmMajor() {
        try {
            byte[] b = asmClass();
            return ((b[6] & 0xff) << 8) | (b[7] & 0xff);
        } catch (IOException e) {
            throw new AssertionError(e);
        }
    }
}
