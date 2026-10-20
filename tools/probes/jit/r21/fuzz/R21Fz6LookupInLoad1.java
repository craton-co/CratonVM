// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 18, lane review-w18: `Lookup.in` loads an enclosing class
// that is not loaded yet (`CRATONVM_LOOKUP_IN_LOADS_OUTER`, wave 16,
// `classloader::lk_class_relation_rooted`).
//
// Risk targeted (docs/known-issues/jit/r21-review-w18-findings-20261019.md):
// JDK 25's `in()` asks `VerifyAccess.isSamePackageMember` while PRIVATE
// survives, and through it `getEnclosingClass()`, which resolves the outer
// class through the nested class's own constant pool (loading, not
// initializing). So on HotSpot:
//   - a missing outer class is that resolution's NoClassDefFoundError, thrown
//     out of `in()`; the failure is a cached constant-pool resolution error
//     (JVMS 5.4.3), so a second `in()` and a `getSimpleName()` throw again
//     WITHOUT asking the loader again (asks(Gone) stays 1);
//   - an outer class that exists but is not loaded is loaded by `in()`;
//   - a spoofed member (the outer class does not list it) is the
//     IncompatibleClassChangeError of `check_for_inner_class`;
//   - without PRIVATE nothing is walked: no load, no error.
// The classes are generated with java.lang.classfile into a loader that
// counts the requests it is asked for and refuses `Gone` / `Gone2`. The
// `rounds` pass repeats the loading `in()` with a fresh loader per round,
// with garbage between rounds, so the load (a GC point under the native's
// rooted references) runs under collections.
//
// Run:
//   javac -d out R21Fz6LookupInLoad1.java
//   java -Xmx256m -cp out R21Fz6LookupInLoad1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz6LookupInLoad1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz6LookupInLoad1
//   cratonvm --nojit -Xmx256m -cp out R21Fz6LookupInLoad1
//   CRATONVM_LOOKUP_IN_LOADS_OUTER=0 cratonvm ...   # control arm (the by-name answer)
// HotSpot expectation (by reading JDK 25 and HotSpot, not yet run):
//   lk modes=15, in(Gone$In) -> NoClassDefFoundError r21fz6li/Gone,
//   asks(Gone)=1 after every step, in(Here$In) -> modes=9, Here loaded=true,
//   in(Spoof$In) -> IncompatibleClassChangeError "r21fz6li.Spoof and
//   r21fz6li.Spoof$In disagree on InnerClasses attribute", lk9 modes=9,
//   lk9.in(Gone2$In) -> modes=9 with asks(Gone2)=0.

import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.InnerClassInfo;
import java.lang.classfile.attribute.InnerClassesAttribute;
import java.lang.constant.ClassDesc;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;

public class R21Fz6LookupInLoad1 {
    static final String P = "r21fz6li.";

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        final Map<String, Integer> asked = new HashMap<>();

        Loader() {
            super(R21Fz6LookupInLoad1.class.getClassLoader());
        }

        int asks(String simple) {
            synchronized (asked) {
                return asked.getOrDefault(P + simple, 0);
            }
        }

        boolean loaded(String simple) {
            return findLoadedClass(P + simple) != null;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.startsWith(P)) {
                return super.loadClass(name, resolve);
            }
            synchronized (asked) {
                asked.merge(name, 1, Integer::sum);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                byte[] b = bytes.get(name);
                if (b == null) {
                    throw new ClassNotFoundException(name);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static InnerClassInfo member(String inner, String outer) {
        return InnerClassInfo.of(ClassDesc.of(P + inner), Optional.of(ClassDesc.of(P + outer)),
                Optional.of("In"), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC);
    }

    static byte[] cls(String name, InnerClassInfo... rows) {
        return ClassFile.of().build(ClassDesc.of(P + name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            if (rows.length > 0) {
                cb.with(InnerClassesAttribute.of(rows));
            }
        });
    }

    static Loader loader() {
        Loader ld = new Loader();
        ld.bytes.put(P + "Sib", cls("Sib"));
        ld.bytes.put(P + "Gone$In", cls("Gone$In", member("Gone$In", "Gone")));
        ld.bytes.put(P + "Gone2$In", cls("Gone2$In", member("Gone2$In", "Gone2")));
        ld.bytes.put(P + "Here$In", cls("Here$In", member("Here$In", "Here")));
        ld.bytes.put(P + "Here", cls("Here", member("Here$In", "Here")));
        ld.bytes.put(P + "Spoof$In", cls("Spoof$In", member("Spoof$In", "Spoof")));
        ld.bytes.put(P + "Spoof", cls("Spoof"));
        return ld;
    }

    static String describe(MethodHandles.Lookup r) {
        Class<?> prev = r.previousLookupClass();
        return "modes=" + r.lookupModes() + " class=" + r.lookupClass().getName() + " prev="
                + (prev == null ? "null" : prev.getName());
    }

    static String tryIn(MethodHandles.Lookup lk, Class<?> target, boolean message) {
        try {
            return describe(lk.in(target));
        } catch (Throwable t) {
            return "EXC " + t.getClass().getName() + (message ? ": " + t.getMessage() : "");
        }
    }

    static String trySimple(Class<?> c) {
        try {
            return "simple=" + c.getSimpleName();
        } catch (Throwable t) {
            return "EXC " + t.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        Loader ld = loader();
        Class<?> sib = Class.forName(P + "Sib", false, ld);
        Class<?> goneIn = Class.forName(P + "Gone$In", false, ld);
        Class<?> gone2In = Class.forName(P + "Gone2$In", false, ld);
        Class<?> hereIn = Class.forName(P + "Here$In", false, ld);
        Class<?> spoofIn = Class.forName(P + "Spoof$In", false, ld);
        MethodHandles.Lookup lk = MethodHandles.privateLookupIn(sib, MethodHandles.lookup());
        System.out.println("lk " + describe(lk));
        System.out.println("before asks(Gone)=" + ld.asks("Gone") + " asks(Here)=" + ld.asks("Here")
                + " Here loaded=" + ld.loaded("Here"));

        System.out.println("in(Gone$In) #1 " + tryIn(lk, goneIn, true));
        System.out.println("asks(Gone)=" + ld.asks("Gone"));
        System.out.println("in(Gone$In) #2 " + tryIn(lk, goneIn, false));
        System.out.println("Gone$In " + trySimple(goneIn));
        System.out.println("asks(Gone)=" + ld.asks("Gone"));

        System.out.println("in(Here$In) " + tryIn(lk, hereIn, true));
        System.out.println("Here asked=" + (ld.asks("Here") > 0) + " loaded=" + ld.loaded("Here"));

        System.out.println("in(Spoof$In) " + tryIn(lk, spoofIn, true));

        MethodHandles.Lookup lk9 = lk.dropLookupMode(MethodHandles.Lookup.PRIVATE);
        System.out.println("lk9 " + describe(lk9));
        System.out.println("lk9.in(Gone2$In) " + tryIn(lk9, gone2In, true));
        System.out.println("asks(Gone2)=" + ld.asks("Gone2"));

        // Hot: the same answers from a compiled caller.
        long modes = 0;
        int ncdfe = 0;
        for (int i = 0; i < 20_000; i++) {
            modes += lk.in(hereIn).lookupModes();
            if ((i & 15) == 0) {
                try {
                    lk.in(goneIn);
                } catch (NoClassDefFoundError e) {
                    ncdfe++;
                }
            }
        }
        System.out.println("hot modes=" + modes + " ncdfe=" + ncdfe + " asks(Gone)=" + ld.asks("Gone"));

        // Rounds: a fresh loader each time, so every `in()` loads `Here`, with
        // garbage between rounds.
        List<byte[]> junk = new ArrayList<>();
        int ok = 0;
        int icce = 0;
        for (int round = 0; round < 40; round++) {
            Loader r = loader();
            Class<?> rs = Class.forName(P + "Sib", false, r);
            Class<?> rh = Class.forName(P + "Here$In", false, r);
            Class<?> rsp = Class.forName(P + "Spoof$In", false, r);
            MethodHandles.Lookup rl = MethodHandles.privateLookupIn(rs, MethodHandles.lookup());
            for (int k = 0; k < 64; k++) {
                junk.add(new byte[16 * 1024]);
            }
            if (junk.size() > 1024) {
                junk.clear();
            }
            MethodHandles.Lookup got = rl.in(rh);
            if (got.lookupModes() == 9 && got.lookupClass() == rh && r.loaded("Here")) {
                ok++;
            }
            try {
                rl.in(rsp);
            } catch (IncompatibleClassChangeError e) {
                icce++;
            }
        }
        System.out.println("rounds ok=" + ok + " icce=" + icce);
    }
}
