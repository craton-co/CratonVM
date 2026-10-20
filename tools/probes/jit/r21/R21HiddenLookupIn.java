// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 10, lane reflect: Lookup.in among a hidden class, its lookup
// class and same-package classes (page
// docs/internal/fixed-bugs/r21-review-w68-native-findings-FIXED-20261019.md item 1).
//
// JDK 25's Lookup.in keeps PACKAGE when VerifyAccess.isSamePackage holds (same
// loader, same getPackageName -- a hidden class `X/0x..` is in X's package) and
// keeps PRIVATE|PROTECTED only when isSamePackageMember holds (the same
// getOutermostEnclosingClass, the getEnclosingClass() chain). It never reads the
// nest host, so a NESTMATE hidden class answers like a non-nestmate one.
//
// Classes: L = R21HiddenLookupIn (the lookup class), I = L's member class Inner,
// M = R21HiddenLookupInMate (another top-level class of the package),
// H / HN = R21HiddenLookupInPayload defined hidden without / with NESTMATE,
// LAM = a lambda's proxy class. Every row prints lookupModes().
//
// Run:
//   javac -d out R21HiddenLookupIn.java
//   java -Xmx256m -cp out R21HiddenLookupIn                        # HotSpot 25
//   cratonvm -Xmx256m -cp out R21HiddenLookupIn                    # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21HiddenLookupIn
//   cratonvm --nojit -Xmx256m -cp out R21HiddenLookupIn
// Kill-switch control: CRATONVM_LOOKUP_IN_PACKAGE_MEMBER=0 -> the `in` rows whose
// lookup or target is hidden differ, except H.in(H) and H.in(HN) (a hidden class lands in a package
// named by its `/0x` tail: 0 instead of 25 for these package-private classes);
// the L / M / I rows and the *.modes / H.in(H) rows are unchanged.
// HotSpot expectation (both modes):
//   hidden H true
//   hidden HN true
//   hidden LAM true
//   L.modes 95
//   L.in(M) 25
//   L.in(I) 31
//   L.in(H) 25
//   L.in(HN) 25
//   L.in(LAM) 25
//   H.modes 95
//   H.in(H) 95
//   H.in(L) 25
//   H.in(M) 25
//   H.in(I) 25
//   H.in(HN) 25
//   HN.modes 95
//   HN.in(L) 25
//   HN.in(M) 25
//   HN.in(I) 25
//   I.in(H) 25
//   L.in(I).in(H) 25
//   L.in(H).in(L) 25
//   LAM.priv 31
//   LAM.in(L) 25
//   LAM.in(H) 25
//   (the HM rows below, then)
//   done
//
// Wave 11 rows (page docs/known-issues/jit/r21-review-w10-findings-20261019.md
// item 3): HM = R21HiddenLookupInOuter$Mem's bytes defined hidden (its
// InnerClasses self entry names the class itself, renamed by the parser to the
// mangled name), O = R21HiddenLookupInOuter. Each row prints its answer or the
// exception class and message (hex tails printed as `/0x..`). HotSpot 25,
// measured by the orchestrator (wave 11), and the expectation in both modes
// since wave 12:
//   hidden HM true
//   HM.declaring java.lang.IncompatibleClassChangeError R21HiddenLookupInOuter and R21HiddenLookupInOuter$Mem/0x.. disagree on InnerClasses attribute
//   HM.enclosing (the same ICCE)
//   HM.modes 95
//   HM.in(O) (the same ICCE)
//   O.in(HM) (the same ICCE)
//   HM.in(MEM) (the same ICCE)
//   MEM.declaring R21HiddenLookupInOuter
//   O.in(MEM) 31
// Kill-switch control: CRATONVM_LOOKUP_IN_PACKAGE_MEMBER=0 -> the three HM `in`
// rows read 0 (the old name split's package) instead of the ICCE.

import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;

public class R21HiddenLookupIn {
    static class Inner {
        static Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    static void row(String name, Lookup l) {
        System.out.println(name + " " + l.lookupModes());
    }

    public static void main(String[] args) throws Throwable {
        Lookup l = MethodHandles.lookup();
        byte[] bytes;
        try (InputStream in = R21HiddenLookupIn.class.getResourceAsStream("R21HiddenLookupInPayload.class")) {
            bytes = in.readAllBytes();
        }
        Lookup hl = l.defineHiddenClass(bytes, true);
        Lookup hnl = l.defineHiddenClass(bytes, true, Lookup.ClassOption.NESTMATE);
        Class<?> h = hl.lookupClass();
        Class<?> hn = hnl.lookupClass();
        Runnable r = () -> { };
        Class<?> lam = r.getClass();
        Class<?> m = R21HiddenLookupInMate.class;
        Class<?> i = Inner.class;

        System.out.println("hidden H " + h.isHidden());
        System.out.println("hidden HN " + hn.isHidden());
        System.out.println("hidden LAM " + lam.isHidden());

        row("L.modes", l);
        row("L.in(M)", l.in(m));
        row("L.in(I)", l.in(i));
        row("L.in(H)", l.in(h));
        row("L.in(HN)", l.in(hn));
        row("L.in(LAM)", l.in(lam));

        row("H.modes", hl);
        row("H.in(H)", hl.in(h));
        row("H.in(L)", hl.in(R21HiddenLookupIn.class));
        row("H.in(M)", hl.in(m));
        row("H.in(I)", hl.in(i));
        row("H.in(HN)", hl.in(hn));

        row("HN.modes", hnl);
        row("HN.in(L)", hnl.in(R21HiddenLookupIn.class));
        row("HN.in(M)", hnl.in(m));
        row("HN.in(I)", hnl.in(i));

        row("I.in(H)", Inner.lookup().in(h));
        row("L.in(I).in(H)", l.in(i).in(h));
        row("L.in(H).in(L)", l.in(h).in(R21HiddenLookupIn.class));

        Lookup laml = MethodHandles.privateLookupIn(lam, l);
        row("LAM.priv", laml);
        row("LAM.in(L)", laml.in(R21HiddenLookupIn.class));
        row("LAM.in(H)", laml.in(h));

        byte[] memBytes;
        try (InputStream in = R21HiddenLookupIn.class.getResourceAsStream("R21HiddenLookupInOuter$Mem.class")) {
            memBytes = in.readAllBytes();
        }
        Lookup hml = l.defineHiddenClass(memBytes, true);
        Class<?> hm = hml.lookupClass();
        Class<?> mem = R21HiddenLookupInOuter.Mem.class;
        Lookup ol = R21HiddenLookupInOuter.lookup();
        System.out.println("hidden HM " + hm.isHidden());
        answer("HM.declaring", () -> name(hm.getDeclaringClass()));
        answer("HM.enclosing", () -> name(hm.getEnclosingClass()));
        row("HM.modes", hml);
        answer("HM.in(O)", () -> String.valueOf(hml.in(R21HiddenLookupInOuter.class).lookupModes()));
        answer("O.in(HM)", () -> String.valueOf(ol.in(hm).lookupModes()));
        answer("HM.in(MEM)", () -> String.valueOf(hml.in(mem).lookupModes()));
        answer("MEM.declaring", () -> name(mem.getDeclaringClass()));
        answer("O.in(MEM)", () -> String.valueOf(ol.in(mem).lookupModes()));
        System.out.println("done");
    }

    interface Answer {
        String get() throws Throwable;
    }

    static String name(Class<?> c) {
        return c == null ? "null" : c.getName().replaceAll("/0x[0-9a-f]+", "/0x..");
    }

    static void answer(String row, Answer a) {
        String out;
        try {
            out = a.get();
        } catch (Throwable t) {
            String msg = t.getMessage() == null ? "null" : t.getMessage().replaceAll("/0x[0-9a-f]+", "/0x..");
            out = t.getClass().getName() + " " + msg;
        }
        System.out.println(row + " " + out);
    }
}

class R21HiddenLookupInOuter {
    static class Mem {
    }

    static Lookup lookup() {
        return MethodHandles.lookup();
    }
}

class R21HiddenLookupInMate {
}

class R21HiddenLookupInPayload {
}
