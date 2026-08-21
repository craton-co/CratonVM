// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Correctness pin for the CP-INDEXED compiled `ldc` (`jit_ldc_string_cp` /
// `jit_ldc_class_cp` answering from JVMS §5.4.3's recorded resolution).
//
// The predecessor helpers re-derived the constant on every execution, so
// "does it still agree" was answered by construction: the string pool and
// the mirror map WERE the answer. Answering from a per-`(class, cp index)`
// record instead makes three things falsifiable, and this probe prints each:
//
//  1. **Identity survives the record.** JVMS §5.1 interns string literals, so
//     two `ldc`s of one CP entry, two CP entries with equal text, and
//     `LITERAL == LITERAL.intern()` must all be `true`. A record that stored
//     an UNINTERNED object would break the second and third while leaving the
//     first true, which is why all three are here.
//
//  2. **Identity survives a moving collection.** The record holds a live
//     reference. If the collector did not remap it, an `ldc` executed after a
//     `System.gc()` would answer with a stale address — a wrong object, or a
//     crash. Every value is re-read after a forced collection AND compared
//     against the pre-collection reference, which is the comparison that
//     fails if the store is remapped but the caller's copy is not (or vice
//     versa).
//
//  3. **A FAILED class resolution is not recorded.** JVMS §5.4.3 requires the
//     error to be raised on EVERY attempt. Recording a failure (or recording
//     a null) would turn the second attempt into a silent success or a null
//     mirror. `missingClass()` is called three times and must throw all three.
//
// Everything is read back inside a hot loop as well as once, so the answers
// come out of compiled code — the door the record was added for — and not
// only out of the interpreter.
//
//   java                                          -cp out LdcConstCacheOracle
//   cratonvm --java-home <jdk>                    -cp out LdcConstCacheOracle
//   CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0 cratonvm ... LdcConstCacheOracle
//
// All three arms must print byte-for-byte the same output.
public final class LdcConstCacheOracle {

    static final String TEXT = "ldc-const-cache-oracle-literal";

    // Two DIFFERENT constant-pool entries in this class (the compiler emits one
    // CONSTANT_String per distinct text, so these two methods share an entry)
    // versus one in a different class (`Other`), which is a genuinely different
    // entry with the same text. Only the second pair can catch a record that
    // forgot to intern.
    static String siteA() { return "ldc-const-cache-oracle-literal"; }
    static String siteB() { return "ldc-const-cache-oracle-literal"; }

    static final class Other {
        static String site() { return "ldc-const-cache-oracle-literal"; }
        static Class<?> mirror() { return java.util.ArrayList.class; }
    }

    static Class<?> classSite() { return java.util.ArrayList.class; }
    static Class<?> classSite2() { return java.util.HashMap.class; }

    /** Resolves to a class that does not exist; must throw on every attempt. */
    static Class<?> missingClass() throws Exception {
        // Not an `ldc` of a missing class — javac would refuse to compile it.
        // `Class.forName` is the reachable way to ask the same question of the
        // same resolver, and the point of the row is the REPEAT, not the door.
        return Class.forName("no.such.Class$LdcConstCacheOracle");
    }

    static long sink;

    public static void main(String[] args) throws Exception {
        // Warm: drive both `ldc` kinds through the tiering thresholds so the
        // values below are produced by compiled code. Its own method, called
        // many times, containing its own loop — the two tier up through
        // different doors (callee compile vs OSR) and the record has to be
        // right at both.
        for (int r = 0; r < 200_000; r++) {
            sink += hotRoundTrip();
        }

        System.out.println("== string identity ==");
        String a1 = siteA();
        String a2 = siteA();
        String b = siteB();
        String other = Other.site();
        System.out.println("sameSiteTwice   = " + (a1 == a2));
        System.out.println("twoSitesOneCls  = " + (a1 == b));
        System.out.println("acrossClasses   = " + (a1 == other));
        System.out.println("equalsIntern    = " + (a1 == a1.intern()));
        System.out.println("freshIntern     = " + (a1 == new String(TEXT).intern()));
        System.out.println("value           = " + a1);

        System.out.println("== class identity ==");
        Class<?> c1 = classSite();
        Class<?> c2 = classSite();
        Class<?> c3 = Other.mirror();
        System.out.println("sameSiteTwice   = " + (c1 == c2));
        System.out.println("acrossClasses   = " + (c1 == c3));
        System.out.println("vsForName       = " + (c1 == Class.forName("java.util.ArrayList")));
        System.out.println("distinctTargets = " + (classSite() != classSite2()));
        System.out.println("value           = " + c1.getName() + " " + classSite2().getName());

        System.out.println("== survives a moving collection ==");
        System.gc();
        System.gc();
        // Re-read through the SAME compiled sites. Comparing against the
        // pre-collection references is the half that catches a store the
        // collector remapped while the pool did not (or the reverse).
        System.out.println("stringSameAfter = " + (siteA() == a1));
        System.out.println("stringStillEq   = " + siteA().equals(TEXT));
        System.out.println("classSameAfter  = " + (classSite() == c1));
        System.out.println("stillInterned   = " + (siteA() == siteA().intern()));
        System.out.println("valueAfter      = " + siteA() + " " + classSite().getName());

        System.out.println("== a failed resolution is raised every time ==");
        for (int i = 1; i <= 3; i++) {
            try {
                Class<?> gone = missingClass();
                System.out.println("attempt " + i + "     = NO EXCEPTION, got " + gone);
            } catch (ClassNotFoundException e) {
                System.out.println("attempt " + i + "     = ClassNotFoundException");
            } catch (NoClassDefFoundError e) {
                System.out.println("attempt " + i + "     = NoClassDefFoundError");
            }
        }

        System.out.println("== hot-loop checksum ==");
        System.out.println("checksum = " + sink);
    }

    /**
     * One execution of every `ldc` kind, folded into a `long`.
     *
     * The identity comparisons are against a NON-CONSTANT receiver on purpose:
     * written as `("lit" == "lit")` javac folds the whole comparison away and
     * the row never runs the opcode under test — the vacuity
     * `LdcStringIdentityProbe` documents at length. `siteA()` is opaque to the
     * folder.
     */
    private static long hotRoundTrip() {
        long acc = 0;
        for (int k = 0; k < 4; k++) {
            acc += siteA().length();
            acc += (siteA() == siteB()) ? 1 : 0;
            acc += classSite().getName().length();
            acc += (classSite() == classSite2()) ? 1 : 0;
        }
        return acc;
    }
}
