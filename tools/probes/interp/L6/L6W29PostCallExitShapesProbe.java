// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29, lane L3: the two post-call exit shapes wave 29
// added (docs/known-issues/interpreter/i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md,
// "Progress (wave 29)"), built like RedefineElidedCtorAfterTheCallProbe (read
// its header first): a compiled body that ELIDED an empty constructor is inside
// a call when that constructor is retransformed into one that counts
// (`made += 1`), and constructs one right after the call returns, before any
// back edge.
//
//   entry   a METHOD-ENTRY body with a loop: `entryBody(idx)`, warmed by
//           100,000 calls with its loop running once per call (so it is
//           compiled at method entry and never OSR'd), then called with idx 1,
//           whose `parkOnE(1)` waits on a latch main releases after the
//           retransform. The loop header comes BEFORE the call in the
//           bytecode, which is what admits the single-pass site
//           (`op_invoke.rs::METHOD_ENTRY_POST_CALL_EXITS_ENABLED`).
//   locked  an OSR loop (the `parked` row of RedefineElidedCtorAfterTheCallProbe)
//           whose call and `new` sit inside `synchronized (LOCK)`: the
//           optimizing OSR-door site of a frame HOLDING A LOCK
//           (`ir_lower.rs::post_call_exit_state`).
// `ctor-ran-after-the-call` says whether the `new` right after the call ran
// the retransformed constructor. Each row prints `true` when the frame left
// for the interpreter at the call's return (or ran interpreted throughout).
//
// HotSpot 25 (agent, JIT on and -Xint alike; JDK 25.0.3 locally, 2026-09-30):
//     entry ctor-ran-after-the-call=true
//     locked ctor-ran-after-the-call=true
// Without the agent: `no agent`.
//
// CratonVM, expected (not run by the lane that wrote it):
// * `--nojit`: HotSpot's lines.
// * `entry` under `CRATONVM_C2_SUPERSEDE=0` (the single-pass method-entry body
//   stays): HotSpot's line because the site fires. Positive control, under
//   `CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1`:
//     `exit polls candidate: L6W29PostCallExitShapesProbe.entryBody... osr=false ... withdrawn=true ... post-call-sites=N` (N >= 1)
//     `post-call exit verdict #k: L6W29PostCallExitShapesProbe.entryBody... named=true withdrawn=true verdict=0x..` (non-zero)
//   With the default settings the body may be the optimizing tier's, whose
//   METHOD-ENTRY compiles get no site yet (the page's remaining item): then
//   `false`, and no `post-call exit verdict` line names `entryBody`. A `true`
//   with no such line means the constructor was not elided in that body (a
//   real call reached the new bytecode), not that the site fired.
// * `locked` with the default settings (optimizing OSR tier): HotSpot's line;
//   the same two dbg lines name `L6W29PostCallExitShapesProbe.locked` with
//   `ir=true`. Under `CRATONVM_JIT_OSR_OPTIMIZING=0` the single-pass tier
//   decides (its sites never refused a held lock on its own; whether its map
//   is transferable there is `safepoint.rs::branch_exit_frame_is_transferable`).
// * Before wave 29: `entry ... =false` under `CRATONVM_C2_SUPERSEDE=0`, and
//   `locked ... =false` by default (the IR site was refused for the held lock:
//   `post-call-sites=0` on its candidate line).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L6W29PostCallExitShapesProbe$Agent
//     Can-Retransform-Classes: true
// containing L6W29PostCallExitShapesProbe*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L6W29PostCallExitShapesProbe
import java.io.ByteArrayOutputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L6W29PostCallExitShapesProbe {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The entry row's class: an empty constructor until retransformed. */
    public static class MarkerE {
        static int made;

        /** Puts the Fieldref `made:I` in the class's own pool, for the rewrite. */
        static int count() {
            return made;
        }
    }

    /** The locked row's class: an empty constructor until retransformed. */
    public static class MarkerL {
        static int made;

        /** Puts the Fieldref `made:I` in the class's own pool, for the rewrite. */
        static int count() {
            return made;
        }
    }

    static final String PREFIX = "L6W29PostCallExitShapesProbe$";

    /**
     * On a RETRANSFORM of MarkerE / MarkerL (never at the initial load),
     * rewrites the empty constructor `aload_0; invokespecial Object.<init>;
     * return` into `aload_0; invokespecial Object.<init>; made += 1; return`.
     */
    static final class Rewrite implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null
                    || !((PREFIX + "MarkerE").equals(className)
                            || (PREFIX + "MarkerL").equals(className))) {
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

    // ---- entry ----------------------------------------------------------

    static final CountDownLatch GO_E = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_E = {OPEN, GO_E};
    static volatile boolean entryIn;
    static volatile int entryResult = -1;

    /** A real call (it has an exception table), as in the parent probe. */
    static void parkOnE(int idx) throws InterruptedException {
        try {
            if (idx == 1) {
                entryIn = true;
            }
            LATCHES_E[idx].await();
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    /**
     * The loop header precedes the call in the bytecode (javac puts a `for`
     * loop's test at its top), so the single-pass walk has filed the header's
     * exit map when it reaches the call. The instruction after the call is a
     * local load (`int n = idx`).
     */
    static int entryBody(int idx) throws InterruptedException {
        int delta = -1;
        for (int j = 0; j < 1; j++) {
            int before = MarkerE.made;
            parkOnE(idx);
            int n = idx;
            MarkerE m = new MarkerE();
            delta = MarkerE.made - before + 0 * n;
        }
        return delta;
    }

    // ---- locked ---------------------------------------------------------

    /** A multiple of 1024, so the call at iteration WARM is made. */
    static final long WARM = 20L << 20;
    static final long AFTER = 2L << 20;
    static final Object LOCK = new Object();
    static final CountDownLatch GO_L = new CountDownLatch(1);
    static final CountDownLatch[] LATCHES_L = {OPEN, GO_L};
    static volatile boolean lockedIn;
    static volatile int[] lockedResult;

    /** 1 at `i == WARM`, else 0, without a branch (0 <= i, WARM < 2^62). */
    static int at(long i) {
        return (int) (((i ^ WARM) - 1) >>> 63);
    }

    static void parkOnL(int idx) throws InterruptedException {
        try {
            LATCHES_L[idx].await();
        } catch (IllegalStateException e) {
            throw e;
        }
    }

    /** {made by the last idx-0 `new`, made by the `new` right after the WARM call}. */
    static int[] locked() throws InterruptedException {
        int[] deltas = new int[2];
        for (long i = 0; i < WARM + AFTER; i++) {
            if ((i & 1023) == 0) {
                int idx = at(i);
                lockedIn |= idx == 1;
                synchronized (LOCK) {
                    int before = MarkerL.made;
                    parkOnL(idx);
                    int n = idx;
                    MarkerL m = new MarkerL();
                    deltas[n] = MarkerL.made - before;
                }
            }
        }
        return deltas;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Rewrite(), true);

        Thread entryWorker = new Thread(() -> {
            try {
                for (int k = 0; k < 100_000; k++) {
                    entryBody(0);
                }
                entryResult = entryBody(1);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        entryWorker.start();
        while (!entryIn) {
            Thread.sleep(1);
        }
        // Parked in `GO_E.await()`, a call made from inside compiled `entryBody`.
        Thread.sleep(50);
        i.retransformClasses(MarkerE.class);
        GO_E.countDown();
        entryWorker.join();
        System.out.println("entry ctor-ran-after-the-call=" + (entryResult == 1));

        Thread lockedWorker = new Thread(() -> {
            try {
                lockedResult = locked();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        lockedWorker.start();
        while (!lockedIn) {
            Thread.sleep(1);
        }
        Thread.sleep(50);
        i.retransformClasses(MarkerL.class);
        GO_L.countDown();
        lockedWorker.join();
        int[] l = lockedResult;
        System.out.println("locked ctor-ran-after-the-call=" + (l[1] == 1));
    }
}
