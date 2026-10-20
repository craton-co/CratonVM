// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: the `Class.getName()` mirror memo
// (`CRATONVM_CLASS_GET_NAME_MIRROR_MEMO`; `lang_class.rs`
// `class_get_name_mirror_memo_hit` / `_note`: a per-thread, 64-line,
// direct-mapped table keyed by mirror ADDRESS, valid for one VM and one
// `gc_address_epoch`, checked against the mirror's `name` slot word).
//
// Risk targeted: the memo's soundness is "within one epoch nothing moves or
// is freed, so an equal address is the same mirror". Class UNLOADING frees
// mirrors: a loader dropped, a collection, and a new class (with a DIFFERENT
// name) whose mirror lands at the freed address would answer the old name if
// the epoch did not move. Each round defines eight classes with fresh names
// (`p.Gen<round>_<k>`, java.lang.classfile) in a fresh loader, asks
// getName() / getSimpleName() / getTypeName() / toString() / the array
// class's name hot (> 64 lines: evictions), drops the loader, collects, and
// moves on; names are printed per round. Also hidden classes (name prefix
// only), two threads asking the same mirrors (per-thread lines), and the
// same NAME defined in two live loaders (two mirrors, equal strings, the
// interned identity HotSpot gives `getName()` across loaders).
//
// Run:
//   javac -d out R21Fz2ReflNameReload1.java
//   java -Xmx256m -cp out R21Fz2ReflNameReload1                          # HotSpot 25
//   CRATONVM_CLASS_GET_NAME_MIRROR_MEMO=1 cratonvm -Xmx256m -cp out R21Fz2ReflNameReload1
//   the same with -XX:+UseG1GC, -XX:+UseZGC and -Xmx32m
//   cratonvm -Xmx256m -cp out R21Fz2ReflNameReload1, cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm. (Whether a class
// is actually unloaded is not printed: only names.)

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Array;
import java.util.ArrayList;
import java.util.List;

public class R21Fz2ReflNameReload1 {
    static final int CALLS = 6_000;

    static final class Loader extends ClassLoader {
        Loader() {
            super(R21Fz2ReflNameReload1.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] bytes(String binaryName) {
        return ClassFile.of().build(ClassDesc.of(binaryName), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC, code -> code.aload(0)
                    .invokespecial(ConstantDescs.CD_Object, "<init>", ConstantDescs.MTD_void).return_());
        });
    }

    static String names(Class<?> c) {
        Class<?> arr = Array.newInstance(c, 0).getClass();
        return c.getName() + "," + c.getSimpleName() + "," + c.getTypeName() + "," + c + "," + arr.getName() + ","
                + arr.getTypeName();
    }

    // The hot getName() site: one call site, many mirrors.
    static int hot(Class<?>[] cs, int calls) {
        int h = 0;
        for (int i = 0; i < calls; i++) {
            h = h * 31 + cs[i % cs.length].getName().hashCode();
        }
        return h;
    }

    public static void main(String[] args) throws Exception {
        List<String> out = new ArrayList<>();
        for (int round = 0; round < 12; round++) {
            Loader ld = new Loader();
            Class<?>[] cs = new Class<?>[8 + 64];
            for (int k = 0; k < 8; k++) {
                String n = "p.Gen" + round + "_" + k;
                cs[k] = ld.define(n, bytes(n));
            }
            // 64 more mirrors, so the 64-line table evicts and refills.
            for (int k = 0; k < 64; k++) {
                cs[8 + k] = k % 2 == 0 ? Array.newInstance(cs[k % 8], new int[1 + k / 16]).getClass()
                        : Array.newInstance(int.class, new int[1 + k / 8]).getClass();
            }
            int h1 = hot(cs, CALLS);
            StringBuilder sb = new StringBuilder("round " + round + " hot=" + h1);
            for (int k = 0; k < 8; k++) {
                sb.append(" | ").append(names(cs[k]));
            }
            out.add(sb.toString());
            // Same name, second live loader: equal strings; identity per HotSpot (interned).
            Loader ld2 = new Loader();
            String n0 = "p.Gen" + round + "_0";
            Class<?> twin = ld2.define(n0, bytes(n0));
            out.add("twin " + round + " sameMirror=" + (twin == cs[0]) + " equalName=" + twin.getName().equals(cs[0].getName())
                    + " sameString=" + (twin.getName() == cs[0].getName()) + " " + hot(new Class<?>[] {twin, cs[0]}, 200));
            ld = null;
            ld2 = null;
            cs = null;
            twin = null;
            System.gc();
        }
        for (String s : out) {
            System.out.println(s);
        }
        // Hidden classes: the name is `<binary name>/0x<address>`; print its prefix.
        MethodHandles.Lookup l = MethodHandles.lookup();
        for (int i = 0; i < 4; i++) {
            Class<?> hc = l.defineHiddenClass(bytes("R21Fz2NameHidden"), false).lookupClass();
            String n = hc.getName();
            int h = hot(new Class<?>[] {hc, String.class}, 2_000);
            System.out.println("hidden " + i + " " + n.substring(0, n.indexOf('/')) + " suffixHex="
                    + n.substring(n.indexOf('/') + 1).startsWith("0x") + " stable=" + (hc.getName() == n) + " "
                    + (h == hot(new Class<?>[] {hc, String.class}, 2_000)));
        }
        // Two threads on the same live mirrors.
        Loader ld = new Loader();
        Class<?>[] shared = new Class<?>[8];
        for (int k = 0; k < 8; k++) {
            shared[k] = ld.define("p.Shared" + k, bytes("p.Shared" + k));
        }
        int[] res = new int[2];
        Thread t1 = new Thread(() -> res[0] = hot(shared, 100_000));
        Thread t2 = new Thread(() -> res[1] = hot(shared, 100_000));
        t1.start();
        t2.start();
        t1.join();
        t2.join();
        System.out.println("threads " + res[0] + " " + res[1] + " " + hot(shared, 100_000) + " " + names(shared[7]));
    }
}
