// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 21, lane L4: two ways a frame running an
// obsolete method (JVMTI RetransformClasses / JEP 109) still saw its class's
// NEW body after wave 19.
//
//   * Wide pool (
//     docs/internal/fixed-bugs/interpreter-L3-untranslatable-obsolete-frames-keep-reading-the-new-pool-RETIRED-20260930.md,
//     case 1): Target has a pool of more than 400 entries (Target.pad). The
//     transformer renames "QqA" to "QqB" in place, so the new pool names
//     "parkQqB" at the index Target.parked's one-byte `ldc` uses, and the old
//     "parkQqA" can only be appended above index 255. The frame parked in
//     Target.parked across the retransform used to be left untranslated and
//     loaded "parkQqB" after the wait.
//   * Lines (docs/internal/fixed-bugs/
//     interpreter-L3-obsolete-frames-report-line-numbers-of-the-current-body-FIXED-20260925.md):
//     the same transformer adds 100 to every LineNumberTable line of Target.
//     The parked frame's stack trace used to report the NEW table's line for
//     its pc (its own line + 100); HotSpot reports the obsolete method's own
//     line. A fresh call reports the new table's line.
//
// HotSpot 25 prints (with the agent):
//     before=parkQqA
//     parked=parkQqA,parkQqA,line=69
//     where=174
// The pre-wave-21 code path gives parked=parkQqA,parkQqB,line=169: the
// parked frame read the new pool and the new line table.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: L7ObsoleteWideLdcAndLines$Agent
//     Can-Retransform-Classes: true
// containing L7ObsoleteWideLdcAndLines*.class, then run
//     java|cratonvm --compatible [--nojit] -javaagent:probe.jar -cp probe.jar L7ObsoleteWideLdcAndLines
// Without the agent (the plain probe runner) both VMs print
//     before=parkQqA
//     no agent
//     parked=parkQqA,parkQqA,line=69
//     where=74
// (The line numbers are this file's own: keep the layout above Target.where
// unchanged, or update them.)
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L7ObsoleteWideLdcAndLines {
    static volatile Instrumentation inst;
    static int transforms;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Target {
        public static String first() {
            return "parkQqA";
        }

        public static String parked(CountDownLatch parked, CountDownLatch go)
                throws InterruptedException {
            String before = "parkQqA";
            parked.countDown();
            go.await();
            String after = "parkQqA";
            int line = new Throwable().getStackTrace()[0].getLineNumber();
            return before + "," + after + ",line=" + line;
        }

        public static int where() {
            return new Throwable().getStackTrace()[0].getLineNumber();
        }

        /** More than 255 constants, all after the ones above. */
        public static String[] pad() {
            return new String[] {
                "p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9",
                "p10", "p11", "p12", "p13", "p14", "p15", "p16", "p17", "p18", "p19",
                "p20", "p21", "p22", "p23", "p24", "p25", "p26", "p27", "p28", "p29",
                "p30", "p31", "p32", "p33", "p34", "p35", "p36", "p37", "p38", "p39",
                "p40", "p41", "p42", "p43", "p44", "p45", "p46", "p47", "p48", "p49",
                "p50", "p51", "p52", "p53", "p54", "p55", "p56", "p57", "p58", "p59",
                "p60", "p61", "p62", "p63", "p64", "p65", "p66", "p67", "p68", "p69",
                "p70", "p71", "p72", "p73", "p74", "p75", "p76", "p77", "p78", "p79",
                "p80", "p81", "p82", "p83", "p84", "p85", "p86", "p87", "p88", "p89",
                "p90", "p91", "p92", "p93", "p94", "p95", "p96", "p97", "p98", "p99",
                "p100", "p101", "p102", "p103", "p104", "p105", "p106", "p107", "p108", "p109",
                "p110", "p111", "p112", "p113", "p114", "p115", "p116", "p117", "p118", "p119",
                "p120", "p121", "p122", "p123", "p124", "p125", "p126", "p127", "p128", "p129",
                "p130", "p131", "p132", "p133", "p134", "p135", "p136", "p137", "p138", "p139",
                "p140", "p141", "p142", "p143", "p144", "p145", "p146", "p147", "p148", "p149",
                "p150", "p151", "p152", "p153", "p154", "p155", "p156", "p157", "p158", "p159",
                "p160", "p161", "p162", "p163", "p164", "p165", "p166", "p167", "p168", "p169",
                "p170", "p171", "p172", "p173", "p174", "p175", "p176", "p177", "p178", "p179",
                "p180", "p181", "p182", "p183", "p184", "p185", "p186", "p187", "p188", "p189",
                "p190", "p191", "p192", "p193", "p194", "p195", "p196", "p197", "p198", "p199",
            };
        }
    }

    /**
     * Odd retransforms rename QqA to QqB and move every line of Target down
     * by 100; even ones hand the original back.
     */
    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"L7ObsoleteWideLdcAndLines$Target".equals(className)) {
                return null;
            }
            transforms++;
            byte[] out = bytes.clone();
            if (transforms % 2 == 0) {
                return out;
            }
            for (int i = 0; i + 2 < out.length; i++) {
                if (out[i] == 'Q' && out[i + 1] == 'q' && out[i + 2] == 'A') {
                    out[i + 2] = 'B';
                }
            }
            shiftLines(out, 100);
            return out;
        }
    }

    static int u2(byte[] b, int p) {
        return ((b[p] & 0xff) << 8) | (b[p + 1] & 0xff);
    }

    static int u4(byte[] b, int p) {
        return (u2(b, p) << 16) | u2(b, p + 2);
    }

    /** Add `delta` to every LineNumberTable line of the class file `b`. */
    static void shiftLines(byte[] b, int delta) {
        int p = 8;
        int count = u2(b, p);
        p += 2;
        int code = -1;
        int lines = -1;
        for (int i = 1; i < count; i++) {
            int tag = b[p] & 0xff;
            switch (tag) {
                case 1: {
                    int len = u2(b, p + 1);
                    String s = new String(b, p + 3, len, StandardCharsets.UTF_8);
                    if (s.equals("Code")) {
                        code = i;
                    } else if (s.equals("LineNumberTable")) {
                        lines = i;
                    }
                    p += 3 + len;
                    break;
                }
                case 3: case 4: case 9: case 10: case 11: case 12: case 17: case 18:
                    p += 5;
                    break;
                case 5: case 6:
                    p += 9;
                    i++;
                    break;
                case 7: case 8: case 16: case 19: case 20:
                    p += 3;
                    break;
                case 15:
                    p += 4;
                    break;
                default:
                    throw new IllegalStateException("constant tag " + tag);
            }
        }
        p += 6; // access_flags, this_class, super_class
        p += 2 + 2 * u2(b, p); // interfaces
        for (int kind = 0; kind < 2; kind++) { // fields, then methods
            int members = u2(b, p);
            p += 2;
            for (int m = 0; m < members; m++) {
                int attributes = u2(b, p + 6);
                p += 8;
                for (int a = 0; a < attributes; a++) {
                    int name = u2(b, p);
                    int len = u4(b, p + 2);
                    if (name == code) {
                        int q = p + 6 + 4; // max_stack, max_locals
                        q += 4 + u4(b, q); // code
                        q += 2 + 8 * u2(b, q); // exception table
                        int inner = u2(b, q);
                        q += 2;
                        for (int c = 0; c < inner; c++) {
                            int innerName = u2(b, q);
                            int innerLen = u4(b, q + 2);
                            if (innerName == lines) {
                                int entries = u2(b, q + 6);
                                for (int e = 0; e < entries; e++) {
                                    int at = q + 8 + 4 * e + 2;
                                    int line = u2(b, at) + delta;
                                    b[at] = (byte) (line >> 8);
                                    b[at + 1] = (byte) line;
                                }
                            }
                            q += 6 + innerLen;
                        }
                    }
                    p += 6 + len;
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("before=" + Target.first());
        if (inst == null) {
            System.out.println("no agent");
        } else {
            inst.addTransformer(new Rename(), true);
        }

        CountDownLatch parked = new CountDownLatch(1);
        CountDownLatch go = new CountDownLatch(1);
        String[] result = new String[1];
        Thread worker = new Thread(() -> {
            try {
                result[0] = Target.parked(parked, go);
            } catch (Throwable t) {
                result[0] = t.toString();
            }
        });
        worker.start();
        parked.await();
        if (inst != null) {
            inst.retransformClasses(Target.class);
        }
        go.countDown();
        worker.join();
        System.out.println("parked=" + result[0]);
        System.out.println("where=" + Target.where());
        if (Target.pad().length != 200) {
            System.out.println("pad=" + Target.pad().length);
        }
    }
}
