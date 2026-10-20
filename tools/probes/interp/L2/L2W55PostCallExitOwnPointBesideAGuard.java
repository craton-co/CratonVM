// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L2: a withdrawn single-pass METHOD-ENTRY
// body leaves right after a call whose FLOATING-POINT result is the last
// argument of the next call (`maker.take(park(idx))`): the call's successor
// is the next invoke itself, whose arm flushes the result from its XMM
// register to its frame word and files its `REEXECUTE` guard snapshot at that
// bci -- a guard of a DIFFERENT frame than the post-call claim's map, which
// saw the result in the register. (An `int`, `long` or reference result is in
// its frame word already, and that claim has been wave 49's `Own`.) Waves
// 48-54 dropped the claim (outcome `Dropped`), so the frame ran the next call
// compiled and left at the site after it. Since wave 55 the claim's stub
// bakes its own point beside the guard and the frame leaves BEFORE the next
// call
// (docs/internal/fixed-bugs/interpreter-L2-proposal-single-pass-post-call-stubs-bake-their-own-point-FIXED-20261020.md;
// `CRATONVM_JIT_POST_CALL_OWN_POINT_EXITS`, outcome `OwnBeside`).
//
// Built like L2W49PostCallExitBesideAGuard (read its header first): a
// compiled body is inside a call when the class of an ELIDED empty
// constructor its next call runs (`take` does `new Marker?()`) is
// retransformed into one that counts (`made += 1`).
//
//   float        `floatMaker.take(parkF(idx))`: a `float`, then `invokevirtual take`.
//   double       `doubleMaker.take(parkD(idx))`: a `double`, the same.
//   float-again  the first shape again, on its own classes and body, last.
// Each body is warmed by 100,000 calls with idx 0 (compiled at method entry),
// then called with idx 1, whose `park?(1)` waits on a latch main releases
// after the retransform. `ctor-ran-in-the-next-call` says whether `take`'s
// `new` ran the retransformed constructor.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 with the agent, default flags; the same
// with -Xint)
// float ctor-ran-in-the-next-call=true
// double ctor-ran-in-the-next-call=true
// float-again ctor-ran-in-the-next-call=true
// HOTSPOT_EXPECTED_END
// Without the agent: `no agent`.
//
// CratonVM (lane's debug build on Windows, `CRATONVM_C2_SUPERSEDE=0`): HotSpot's
// lines with the switch on AND off. The rows are a conformance check, not a
// discriminator: in this tier the next call goes through the dispatch helper,
// which re-dispatches to the current (withdrawn, so interpreted) `take`
// whether or not the frame left before it; the shape that would print `false`
// with the switch off is a guarded-virtual SPLICE of the redefined class as
// the next call, which this lane could not make the single-pass walk produce
// after a real call (a scratch variant whose `take` read a retransformed
// `Callee.value()` printed `true` both ways too).
//   POSITIVE CONTROL, under `CRATONVM_C2_SUPERSEDE=0 CRATONVM_DBG_DEOPT=1`:
//     `post-call exit claim at bci=11 of L2W55PostCallExitOwnPointBesideAGuard.floatBody:(I)I: OwnBeside
//      (points there: ["ReceiverTypeChanged/reexecute"])`
//   (and `doubleBody`, `againBody`), then
//     `post-call exit verdict #k: L2W55PostCallExitOwnPointBesideAGuard.floatBody:(I)I ... named=true withdrawn=true verdict=0x7`.
//   With `CRATONVM_JIT_POST_CALL_OWN_POINT_EXITS=0` the same claims print
//   `Dropped` (measured: both lines seen on the lane's build).
// * `--nojit`: HotSpot's lines.
// * JIT on, default settings: the body may be the optimizing tier's, whose
//   post-call sites are wave 38's.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W55PostCallExitOwnPointBesideAGuard$Agent
//     Can-Retransform-Classes: true
// containing L2W55PostCallExitOwnPointBesideAGuard*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W55PostCallExitOwnPointBesideAGuard
import java.io.ByteArrayOutputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L2W55PostCallExitOwnPointBesideAGuard {
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

    static final String PREFIX = "L2W55PostCallExitOwnPointBesideAGuard$Marker";

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

    // ---- float: `floatMaker.take(parkF(idx))` --------------------------------

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

    static int floatBody(int idx) throws InterruptedException {
        int before = MarkerF.made;
        int seen = floatMaker.take(parkF(idx));
        return MarkerF.made - before + (seen - idx);
    }

    // ---- double: `doubleMaker.take(parkD(idx))` ------------------------------

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

    static int doubleBody(int idx) throws InterruptedException {
        int before = MarkerD.made;
        int seen = doubleMaker.take(parkD(idx));
        return MarkerD.made - before + (seen - idx);
    }

    // ---- float again, on its own classes and body ----------------------------

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

    static int againBody(int idx) throws InterruptedException {
        int before = MarkerA.made;
        int seen = againMaker.take(parkA(idx));
        return MarkerA.made - before + (seen - idx);
    }


    interface Body {
        int run(int idx) throws InterruptedException;
    }

    static boolean row(Instrumentation i, Body body, Class<?> marker, CountDownLatch go,
            java.util.function.BooleanSupplier in) throws Exception {
        int[] out = {-1};
        Thread worker = new Thread(() -> {
            try {
                for (int k = 0; k < 100_000; k++) {
                    body.run(0);
                }
                out[0] = body.run(1);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        worker.start();
        while (!in.getAsBoolean()) {
            Thread.sleep(1);
        }
        // Parked in `go.await()`, a call made from inside the compiled body.
        Thread.sleep(50);
        i.retransformClasses(marker);
        go.countDown();
        worker.join();
        return out[0] == 1;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Rewrite(), true);
        System.out.println("float ctor-ran-in-the-next-call="
                + row(i, L2W55PostCallExitOwnPointBesideAGuard::floatBody, MarkerF.class, GO_F,
                        () -> floatIn));
        System.out.println("double ctor-ran-in-the-next-call="
                + row(i, L2W55PostCallExitOwnPointBesideAGuard::doubleBody, MarkerD.class, GO_D,
                        () -> doubleIn));
        System.out.println("float-again ctor-ran-in-the-next-call="
                + row(i, L2W55PostCallExitOwnPointBesideAGuard::againBody, MarkerA.class, GO_A,
                        () -> againIn));
    }
}
