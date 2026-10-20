// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L2: the OSR-tier twin of
// L2W55PostCallExitOwnPointBesideAGuard (read its header first). A withdrawn
// single-pass OSR body leaves right after a call whose FLOATING-POINT result
// is the last argument of the next call (`maker.take(park(idx))`), inside a
// hot loop entered through the OSR door. Waves 48-55 dropped that claim in an
// OSR-tier compile (outcome `Dropped`): the frame ran the next call compiled
// and left at the site after it. Since wave 56 the claim's stub bakes its own
// point there too, and the OSR door's by-bci readers know that point
// (docs/internal/fixed-bugs/interpreter-L2-proposal-single-pass-post-call-stubs-bake-their-own-point-FIXED-20261020.md;
// `CRATONVM_JIT_OSR_POST_CALL_OWN_POINT_EXITS`, outcome `OwnBeside`).
//
//   float        `floatLoop`: `floatMaker.take(parkF(idx))`, a `float`.
//   double       `doubleLoop`: `doubleMaker.take(parkD(idx))`, a `double`.
//   float-again  the first shape again, on its own classes and loop, last.
// Each loop runs 200,000 iterations in ONE call (so it is entered compiled
// through the OSR door); its last iteration passes idx 1, whose `park?(1)`
// waits on a latch main releases after it retransformed the class of the
// ELIDED empty constructor `take` and the loop itself run (`new Marker?()`)
// into one that counts (`made += 1`); the loop's own `new` is what makes the
// retransform withdraw its OSR body. `counted-ctors-in-the-last-iteration`
// is how many of the last iteration's two constructors (`take`'s, then the
// loop's) ran the new one.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 with the agent, default flags; the same
// with -Xint)
// float counted-ctors-in-the-last-iteration=2
// double counted-ctors-in-the-last-iteration=2
// float-again counted-ctors-in-the-last-iteration=2
// HOTSPOT_EXPECTED_END
// Without the agent: `no agent`.
//
// CratonVM, measured on the lane's debug build: HotSpot's lines with the
// switch on AND off, both modes, JIT and `--nojit` (as for the wave-55 probe
// the rows are a conformance check, not a discriminator: the next call goes
// through the dispatch helper, which re-dispatches to the current `take`).
// The default OSR door is the optimizing tier's (its post-call exits are
// wave 28's); the single-pass OSR door needs `CRATONVM_JIT_OSR_OPTIMIZING=0`.
//   POSITIVE CONTROL, under
//   `CRATONVM_C2_SUPERSEDE=0 CRATONVM_JIT_OSR_OPTIMIZING=0 CRATONVM_DBG_DEOPT=1`:
//     `post-call exit claim at bci=34 of L2W56OsrPostCallExitOwnPointBesideAGuard.floatLoop:(I)I: OwnBeside
//      (points there: ["ReceiverTypeChanged/reexecute"])`
//   from the loop's OSR-tier compile (and `doubleLoop`, `againLoop`), then
//     `OSR-exit TRANSFER into live frame: resume bci=34 (6 locals, 2 stack, entry_pc=4)`.
//   With `CRATONVM_JIT_OSR_POST_CALL_OWN_POINT_EXITS=0` the claims print
//   `Dropped` and the transfer resumes at `bci=37`, after the next call. With
//   `CRATONVM_DBG_JIT_METHOD_STATS=1` the OSR lifecycle line reads
//   `osr_exit_at_post_call_own_point=3 osr_exit_map_missing=0` (switch on).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W56OsrPostCallExitOwnPointBesideAGuard$Agent
//     Can-Retransform-Classes: true
// containing L2W56OsrPostCallExitOwnPointBesideAGuard*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W56OsrPostCallExitOwnPointBesideAGuard
import java.io.ByteArrayOutputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L2W56OsrPostCallExitOwnPointBesideAGuard {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The float row's class: an empty constructor until retransformed. */
    public static class MarkerF {
        static int made;

        /** Puts the Fieldref `made:I` in the class's own pool, for the rewrite. */
        static int count() {
            return made;
        }
    }

    /** The double row's class. */
    public static class MarkerD {
        static int made;

        /** Puts the Fieldref `made:I` in the class's own pool, for the rewrite. */
        static int count() {
            return made;
        }
    }

    /** The repeated float row's class. */
    public static class MarkerA {
        static int made;

        static int count() {
            return made;
        }
    }

    static final String PREFIX = "L2W56OsrPostCallExitOwnPointBesideAGuard$Marker";

    /**
     * On a RETRANSFORM of a marker class (never at the initial load), rewrites
     * the empty constructor `aload_0; invokespecial Object.<init>; return`
     * into `aload_0; invokespecial Object.<init>; made += 1; return`.
     */
    static final class Rewrite implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null || className == null || !className.startsWith(PREFIX)) {
                return null;
            }
            try {
                return countingConstructor(bytes, className);
            } catch (RuntimeException e) {
                e.printStackTrace();
                return null;
            }
        }
    }

    /** Offset past the constant-pool entry with tag `tag` at `at` (tag byte included). */
    static int skipEntry(ByteBuffer b, int at, int tag) {
        switch (tag) {
            case 1: return at + 3 + (b.getShort(at + 1) & 0xFFFF);
            case 3: case 4: case 9: case 10: case 11: case 12: case 17: case 18: return at + 5;
            case 5: case 6: return at + 9;
            case 7: case 8: case 16: case 19: case 20: return at + 3;
            case 15: return at + 4;
            default: throw new IllegalStateException("constant tag " + tag);
        }
    }

    static byte[] countingConstructor(byte[] bytes, String className) {
        ByteBuffer b = ByteBuffer.wrap(bytes);
        int count = b.getShort(8) & 0xFFFF;
        int[] offset = new int[count];
        int at = 10;
        for (int i = 1; i < count; i++) {
            offset[i] = at;
            int tag = bytes[at];
            at = skipEntry(b, at, tag);
            if (tag == 5 || tag == 6) {
                i++;
            }
        }
        int field = 0;
        for (int i = 1; i < count; i++) {
            int o = offset[i];
            if (o == 0 || bytes[o] != 9) {
                continue;
            }
            int cls = b.getShort(o + 1) & 0xFFFF;
            int nat = b.getShort(o + 3) & 0xFFFF;
            String owner = utf8(b, offset, b.getShort(offset[cls] + 1) & 0xFFFF);
            String name = utf8(b, offset, b.getShort(offset[nat] + 1) & 0xFFFF);
            String desc = utf8(b, offset, b.getShort(offset[nat] + 3) & 0xFFFF);
            if (owner.equals(className) && name.equals("made") && desc.equals("I")) {
                field = i;
            }
        }
        if (field == 0) {
            throw new IllegalStateException("no Fieldref made:I in " + className);
        }
        at += 6;
        int interfaces = b.getShort(at) & 0xFFFF;
        at += 2 + 2 * interfaces;
        int fields = b.getShort(at) & 0xFFFF;
        at += 2;
        for (int f = 0; f < fields; f++) {
            at = skipMember(b, at);
        }
        int methods = b.getShort(at) & 0xFFFF;
        at += 2;
        for (int m = 0; m < methods; m++) {
            String name = utf8(b, offset, b.getShort(at + 2) & 0xFFFF);
            String desc = utf8(b, offset, b.getShort(at + 4) & 0xFFFF);
            int attrs = b.getShort(at + 6) & 0xFFFF;
            int a = at + 8;
            for (int k = 0; k < attrs; k++) {
                int len = b.getInt(a + 2);
                String attr = utf8(b, offset, b.getShort(a) & 0xFFFF);
                if (name.equals("<init>") && desc.equals("()V") && attr.equals("Code")) {
                    int maxLocals = b.getShort(a + 8) & 0xFFFF;
                    int codeLen = b.getInt(a + 10);
                    int code = a + 14;
                    if (codeLen != 5 || bytes[code] != 0x2a || bytes[code + 1] != (byte) 0xb7
                            || bytes[code + 4] != (byte) 0xb1) {
                        return null;
                    }
                    byte hi = (byte) (field >> 8);
                    byte lo = (byte) field;
                    byte[] body = {
                        0x2a, (byte) 0xb7, bytes[code + 2], bytes[code + 3],
                        (byte) 0xb2, hi, lo,
                        0x04, 0x60,
                        (byte) 0xb3, hi, lo,
                        (byte) 0xb1,
                    };
                    ByteArrayOutputStream out = new ByteArrayOutputStream();
                    out.write(bytes, 0, a + 2);
                    ByteBuffer attrBytes = ByteBuffer.allocate(4 + 2 + 2 + 4 + body.length + 2 + 2);
                    attrBytes.putInt(2 + 2 + 4 + body.length + 2 + 2);
                    attrBytes.putShort((short) 2);
                    attrBytes.putShort((short) maxLocals);
                    attrBytes.putInt(body.length);
                    attrBytes.put(body);
                    attrBytes.putShort((short) 0);
                    attrBytes.putShort((short) 0);
                    out.write(attrBytes.array(), 0, attrBytes.position());
                    int end = a + 6 + len;
                    out.write(bytes, end, bytes.length - end);
                    return out.toByteArray();
                }
                a += 6 + len;
            }
            at = a;
        }
        throw new IllegalStateException("no <init>()V in " + className);
    }

    static int skipMember(ByteBuffer b, int at) {
        int attrs = b.getShort(at + 6) & 0xFFFF;
        int a = at + 8;
        for (int k = 0; k < attrs; k++) {
            a += 6 + b.getInt(a + 2);
        }
        return a;
    }

    static String utf8(ByteBuffer b, int[] offset, int index) {
        int o = offset[index];
        int len = b.getShort(o + 1) & 0xFFFF;
        return new String(b.array(), o + 3, len, StandardCharsets.UTF_8);
    }

    static final CountDownLatch OPEN = new CountDownLatch(0);

    /**
     * Small enough to be spliced into the bodies below (no exception table):
     * the splice copies `new MarkerF()` with its empty constructor elided.
     */
    static class FloatMaker {
        int take(float x) {
            new MarkerF();
            return (int) x;
        }
    }

    /** The same, over a `double` and its own marker. */
    static class DoubleMaker {
        int take(double x) {
            new MarkerD();
            return (int) x;
        }
    }

    /** The same again, for the repeated row. */
    static class AgainMaker {
        int take(float x) {
            new MarkerA();
            return (int) x;
        }
    }

    /** Not final: each body loads its receiver before the park. */
    static FloatMaker floatMaker = new FloatMaker();
    static DoubleMaker doubleMaker = new DoubleMaker();
    static AgainMaker againMaker = new AgainMaker();

    static final int ITERATIONS = 200_000;

    // ---- float: `floatMaker.take(parkF(idx))` in a loop ----------------------

    static final CountDownLatch GO_F = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_F = {OPEN, GO_F};
    static volatile boolean floatIn;

    /** A real call (its exception table keeps it out of every splice). */
    static float parkF(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                floatIn = true;
            }
            LATCHES_F[idx].await();
            return idx;
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    static int floatLoop(int n) throws InterruptedException {
        int ran = 0;
        for (int k = 0; k < n; k++) {
            int idx = k == n - 1 ? 1 : 0;
            int before = MarkerF.made;
            int seen = floatMaker.take(parkF(idx));
            new MarkerF();
            ran = MarkerF.made - before + (seen - idx);
        }
        return ran;
    }

    // ---- double: `doubleMaker.take(parkD(idx))` in a loop --------------------

    static final CountDownLatch GO_D = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_D = {OPEN, GO_D};
    static volatile boolean doubleIn;

    static double parkD(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                doubleIn = true;
            }
            LATCHES_D[idx].await();
            return idx;
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    static int doubleLoop(int n) throws InterruptedException {
        int ran = 0;
        for (int k = 0; k < n; k++) {
            int idx = k == n - 1 ? 1 : 0;
            int before = MarkerD.made;
            int seen = doubleMaker.take(parkD(idx));
            new MarkerD();
            ran = MarkerD.made - before + (seen - idx);
        }
        return ran;
    }

    // ---- float again, on its own classes and loop ----------------------------

    static final CountDownLatch GO_A = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_A = {OPEN, GO_A};
    static volatile boolean againIn;

    static float parkA(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                againIn = true;
            }
            LATCHES_A[idx].await();
            return idx;
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    static int againLoop(int n) throws InterruptedException {
        int ran = 0;
        for (int k = 0; k < n; k++) {
            int idx = k == n - 1 ? 1 : 0;
            int before = MarkerA.made;
            int seen = againMaker.take(parkA(idx));
            new MarkerA();
            ran = MarkerA.made - before + (seen - idx);
        }
        return ran;
    }

    interface Loop {
        int run(int n) throws InterruptedException;
    }

    static int row(Instrumentation i, Loop loop, Class<?> marker, CountDownLatch go,
            java.util.function.BooleanSupplier in) throws Exception {
        int[] out = {-1};
        Thread worker = new Thread(() -> {
            try {
                out[0] = loop.run(ITERATIONS);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        worker.start();
        while (!in.getAsBoolean()) {
            Thread.sleep(1);
        }
        // Parked in `go.await()`, a call made from inside the compiled loop.
        Thread.sleep(50);
        i.retransformClasses(marker);
        go.countDown();
        worker.join();
        return out[0];
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Rewrite(), true);
        System.out.println("float counted-ctors-in-the-last-iteration="
                + row(i, L2W56OsrPostCallExitOwnPointBesideAGuard::floatLoop, MarkerF.class, GO_F,
                        () -> floatIn));
        System.out.println("double counted-ctors-in-the-last-iteration="
                + row(i, L2W56OsrPostCallExitOwnPointBesideAGuard::doubleLoop, MarkerD.class, GO_D,
                        () -> doubleIn));
        System.out.println("float-again counted-ctors-in-the-last-iteration="
                + row(i, L2W56OsrPostCallExitOwnPointBesideAGuard::againLoop, MarkerA.class, GO_A,
                        () -> againIn));
    }
}
