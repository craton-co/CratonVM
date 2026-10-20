// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L2: a SYNCHRONIZED method's compiled loop
// leaves for the interpreter, still holding the method monitor, when the class
// of an ELIDED empty constructor it runs is retransformed (shape 3's
// `self-lock` refusal of
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md;
// `CRATONVM_JIT_SELF_LOCK_MODE_EXIT_HANDOVER`).
//
// A synchronized method with a loop, no exception table and no
// `monitorenter` (`Hashtable.get`, `Vector.indexOf`, `BufferedInputStream.read`:
// 68 such methods in java.base) gets a single-pass SELF-LOCKING body, which
// takes the monitor in its prologue. Until wave 56 none of its loops could
// leave (`[jit-mode-exit-refused] self-lock` under CRATONVM_DBG_JITC=1): the
// activation finished compiled, running the old constructor.
//
//   instance        `synchronized long spin(n, parkAt)` on its own class.
//   static          `static synchronized long spinS(n, parkAt)`.
//   instance-again  the first row again, on its own classes, last.
// Each method is warmed by 20,000 calls (compiled at method entry), then called
// with n=1000, parkAt=500: iteration 500 sets a flag and spins on a volatile
// until main lets it go. Main retransforms the marker class (`new Marker?()`
// in the loop, its empty constructor rewritten into one that counts), then a
// contender thread tries to enter the method's monitor; after 200 ms main
// records whether it got in, then releases the spin.
//   held-while-spinning     the contender did not get the monitor while the
//                           loop (compiled or resumed) still ran;
//   counted-ctors-after     iterations 501..999 ran the new constructor (499);
//   released-after-return   the contender got the monitor once spin returned.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 with the agent, default flags; the same
// with -Xint)
// instance held-while-spinning=true counted-ctors-after=499 released-after-return=true sum=499500
// static held-while-spinning=true counted-ctors-after=499 released-after-return=true sum=499500
// instance-again held-while-spinning=true counted-ctors-after=499 released-after-return=true sum=499500
// HOTSPOT_EXPECTED_END
// Without the agent: `no agent`.
//
// CratonVM, measured on the lane's debug build (Windows), both modes:
// `--nojit` prints HotSpot's lines. JIT on (default, and
// `CRATONVM_C2_SUPERSEDE=0`) with the switch OFF: `counted-ctors-after=0` on
// the `instance` and `instance-again` rows (the self-locking body finishes
// its activation compiled; `[jit-mode-exit-refused] self-lock back-edge
// pc=35 ... method=...BoxI.spin:(II)J` under CRATONVM_DBG_JITC=1). With the
// switch ON: HotSpot's lines (5 runs default, 2 each `--compatible` and
// `CRATONVM_C2_SUPERSEDE=0`). The `static` row is not discriminating here:
// `spinS` is compiled as a wrapped body (its door holds the monitor), whose
// polls always had their exits.
//   POSITIVE CONTROL, under `CRATONVM_DBG_DEOPT=1`:
//     `withdrawn body told to leave: ...BoxI.spin:(II)J verdict=0x7`, then
//     `sink=execute-first-call-tierup running=...BoxI.spin(II)J stash=...BoxI.spin:(II)J bci=35`
//   (the inner spin loop's back edge), and no `self-lock` refusal line.
// RACY: the contender's 200 ms window. Run it 5 times per mode.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W56SelfLockLoopLeavesHoldingTheMonitor$Agent
//     Can-Retransform-Classes: true
// containing L2W56SelfLockLoopLeavesHoldingTheMonitor*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W56SelfLockLoopLeavesHoldingTheMonitor
import java.io.ByteArrayOutputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;

public class L2W56SelfLockLoopLeavesHoldingTheMonitor {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The instance row's class: an empty constructor until retransformed. */
    public static class MarkerI {
        static int made;

        /** Puts the Fieldref `made:I` in the class's own pool, for the rewrite. */
        static int count() {
            return made;
        }
    }

    /** The static row's class. */
    public static class MarkerS {
        static int made;

        static int count() {
            return made;
        }
    }

    /** The repeated row's class. */
    public static class MarkerA {
        static int made;

        static int count() {
            return made;
        }
    }

    static final String PREFIX = "L2W56SelfLockLoopLeavesHoldingTheMonitor$Marker";

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

    static volatile boolean inSpin;
    static volatile boolean go;

    /** The instance row: a self-locking body (no exception table, no monitorenter). */
    static class BoxI {
        synchronized long spin(int n, int parkAt) {
            long s = 0;
            for (int k = 0; k < n; k++) {
                new MarkerI();
                s += k;
                if (k == parkAt) {
                    inSpin = true;
                    while (!go) {
                    }
                }
            }
            return s;
        }
    }

    /** The static row. */
    static class BoxS {
        static synchronized long spinS(int n, int parkAt) {
            long s = 0;
            for (int k = 0; k < n; k++) {
                new MarkerS();
                s += k;
                if (k == parkAt) {
                    inSpin = true;
                    while (!go) {
                    }
                }
            }
            return s;
        }
    }

    /** The repeated instance row. */
    static class BoxA {
        synchronized long spin(int n, int parkAt) {
            long s = 0;
            for (int k = 0; k < n; k++) {
                new MarkerA();
                s += k;
                if (k == parkAt) {
                    inSpin = true;
                    while (!go) {
                    }
                }
            }
            return s;
        }
    }

    interface Spin {
        long run(int n, int parkAt);
    }

    static String row(Instrumentation i, Spin spin, Object monitor, Class<?> marker,
            java.util.function.IntSupplier made) throws Exception {
        inSpin = false;
        go = false;
        long[] sum = {-1};
        int[] before = {0};
        Thread worker = new Thread(() -> {
            for (int k = 0; k < 20_000; k++) {
                spin.run(50, -1);
            }
            before[0] = made.getAsInt();
            sum[0] = spin.run(1000, 500);
        });
        worker.start();
        while (!inSpin) {
            Thread.sleep(1);
        }
        Thread.sleep(50);
        i.retransformClasses(marker);
        boolean[] entered = {false};
        Thread contender = new Thread(() -> {
            synchronized (monitor) {
                entered[0] = true;
            }
        });
        contender.start();
        Thread.sleep(200);
        boolean held = !entered[0];
        go = true;
        worker.join();
        contender.join();
        return "held-while-spinning=" + held
                + " counted-ctors-after=" + (made.getAsInt() - before[0])
                + " released-after-return=" + entered[0]
                + " sum=" + sum[0];
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Rewrite(), true);
        BoxI boxI = new BoxI();
        System.out.println("instance " + row(i, boxI::spin, boxI, MarkerI.class, () -> MarkerI.made));
        System.out.println("static " + row(i, BoxS::spinS, BoxS.class, MarkerS.class, () -> MarkerS.made));
        BoxA boxA = new BoxA();
        System.out.println("instance-again " + row(i, boxA::spin, boxA, MarkerA.class, () -> MarkerA.made));
    }
}
