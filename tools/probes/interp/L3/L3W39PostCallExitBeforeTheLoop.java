// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 39, lane L3: the post-call exit of a METHOD-ENTRY
// single-pass body whose call comes BEFORE any loop-boundary exit map
// (docs/known-issues/interpreter/i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md,
// "Progress (wave 39)"). Built like L6W29PostCallExitShapesProbe (read its
// header first): a compiled body that ELIDED an empty constructor is inside a
// call when that constructor is retransformed into one that counts
// (`made += 1`), and constructs one right after the call returns.
//
//   free    a loop-free body: `freeBody(idx)`.
//   before  the call comes before the body's first loop header in bytecode
//           order: `beforeBody(idx)` (its loop follows the `new`).
// Each body is warmed by 100,000 calls with idx 0 (compiled at method entry),
// then called with idx 1, whose `parkOnX(1)` waits on a latch main releases
// after the retransform. `ctor-ran-after-the-call` says whether the `new`
// right after the call ran the retransformed constructor: `true` when the
// frame left for the interpreter at the call's return (or ran interpreted).
//
// HotSpot 25 (agent, JIT on and -Xint alike; JDK 25.0.3 locally):
//     free ctor-ran-after-the-call=true
//     before ctor-ran-after-the-call=true
// Without the agent: `no agent`.
//
// CratonVM, expected (not run by the lane that wrote it):
// * `--nojit`: HotSpot's lines.
// * JIT on, `CRATONVM_C2_SUPERSEDE=0` (the single-pass method-entry body
//   stays): HotSpot's lines, because the site fires
//   (`op_invoke.rs::METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED`). Before
//   wave 39 both rows printed `false` there: the single-pass walk gave a call
//   no site until a loop header had filed a map. POSITIVE CONTROL, under
//   `CRATONVM_C2_SUPERSEDE=0 CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1`:
//     `exit polls candidate: L3W39PostCallExitBeforeTheLoop.freeBody... withdrawn=true ... post-call-sites=N` (N >= 1)
//     `post-call exit verdict #k: L3W39PostCallExitBeforeTheLoop.freeBody... named=true withdrawn=true verdict=0x..` (non-zero)
//   and the same two lines for `beforeBody`.
// * JIT on, default settings: the body may be the optimizing tier's, whose
//   method-entry sites are wave 38's (`ir_lower.rs::maybe_emit_post_call_exit_site`,
//   `IR_ENTRY_POST_CALL_EXITS_ENABLED`); the same two dbg lines then name the
//   body with `ir=true`. A `true` with no verdict line means the constructor
//   was not elided in that body (a real call reached the new bytecode), not
//   that the site fired.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W39PostCallExitBeforeTheLoop$Agent
//     Can-Retransform-Classes: true
// containing L3W39PostCallExitBeforeTheLoop*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W39PostCallExitBeforeTheLoop
import java.io.ByteArrayOutputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L3W39PostCallExitBeforeTheLoop {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The free row's class: an empty constructor until retransformed. */
    public static class MarkerF {
        static int made;

        /** Puts the Fieldref `made:I` in the class's own pool, for the rewrite. */
        static int count() {
            return made;
        }
    }

    /** The before row's class: an empty constructor until retransformed. */
    public static class MarkerB {
        static int made;

        /** Puts the Fieldref `made:I` in the class's own pool, for the rewrite. */
        static int count() {
            return made;
        }
    }

    static final String PREFIX = "L3W39PostCallExitBeforeTheLoop$";

    /**
     * On a RETRANSFORM of MarkerF / MarkerB (never at the initial load),
     * rewrites the empty constructor `aload_0; invokespecial Object.<init>;
     * return` into `aload_0; invokespecial Object.<init>; made += 1; return`.
     */
    static final class Rewrite implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null
                    || !((PREFIX + "MarkerF").equals(className)
                            || (PREFIX + "MarkerB").equals(className))) {
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

    // ---- free -----------------------------------------------------------

    static final CountDownLatch GO_F = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_F = {OPEN, GO_F};
    static volatile boolean freeIn;

    /** A real call (it has an exception table), as in the parent probes. */
    static void parkOnF(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                freeIn = true;
            }
            LATCHES_F[idx].await();
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    /** No loop: the call's successor (`int n = idx`, a local load) files the body's first map. */
    static int freeBody(int idx) throws InterruptedException {
        int before = MarkerF.made;
        parkOnF(idx);
        int n = idx;
        MarkerF m = new MarkerF();
        return MarkerF.made - before + 0 * n;
    }

    // ---- before ---------------------------------------------------------

    static final CountDownLatch GO_B = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_B = {OPEN, GO_B};
    static volatile boolean beforeIn;

    static void parkOnB(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                beforeIn = true;
            }
            LATCHES_B[idx].await();
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    /** The call precedes the loop header in the bytecode. */
    static int beforeBody(int idx) throws InterruptedException {
        int before = MarkerB.made;
        parkOnB(idx);
        int n = idx;
        MarkerB m = new MarkerB();
        int delta = MarkerB.made - before + 0 * n;
        for (int j = 0; j < 1; j++) {
            delta += 0 * j;
        }
        return delta;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Rewrite(), true);

        int[] free = {-1};
        Thread freeWorker = new Thread(() -> {
            try {
                for (int k = 0; k < 100_000; k++) {
                    freeBody(0);
                }
                free[0] = freeBody(1);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        freeWorker.start();
        while (!freeIn) {
            Thread.sleep(1);
        }
        // Parked in `GO_F.await()`, a call made from inside compiled `freeBody`.
        Thread.sleep(50);
        i.retransformClasses(MarkerF.class);
        GO_F.countDown();
        freeWorker.join();
        System.out.println("free ctor-ran-after-the-call=" + (free[0] == 1));

        int[] before = {-1};
        Thread beforeWorker = new Thread(() -> {
            try {
                for (int k = 0; k < 100_000; k++) {
                    beforeBody(0);
                }
                before[0] = beforeBody(1);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        beforeWorker.start();
        while (!beforeIn) {
            Thread.sleep(1);
        }
        Thread.sleep(50);
        i.retransformClasses(MarkerB.class);
        GO_B.countDown();
        beforeWorker.join();
        System.out.println("before ctor-ran-after-the-call=" + (before[0] == 1));
    }
}
