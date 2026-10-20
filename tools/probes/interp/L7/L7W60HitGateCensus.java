// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L7: the process gates the interpreter
// asked on its HIT paths move into per-VM config, read once
// (`VmConfig::site_census` for `CRATONVM_DBG_FIELD_SITE`, `invoke_stats` for
// `CRATONVM_DBG_INVOKESTATS`, `descriptor_facts` for
// `CRATONVM_JIT_NO_DESCRIPTOR_FACTS`, `jit_enabled` for
// `CRATONVM_DISABLE_JIT` / `--nojit`, `jit_invocation_threshold` for
// `CRATONVM_JIT_THRESHOLD`; the dispatch loop hoists `site_census` with its
// other per-entry gates). No behaviour change: every line is the same in
// every mode. Each shape runs cold first and then in a loop:
//
//   field   quickened `getfield` / `putfield` (and the fused `aload_0; getfield`)
//   array   `arraylength` (fused and unfused), `aaload`, `aastore`
//   door    `invokestatic` of a plain and an empty static body, a private
//           instance call and a `super` call (the special door), a trivial
//           constructor and a field-store constructor
//   cast    a monomorphic `checkcast` / `instanceof`
//
// Positive controls (debug `cratonvm`, `--nojit` and default):
// * `CRATONVM_DBG_FIELD_SITE=1`: the FINAL `[site-cache]` line has non-zero
//   `fast-field: get hit=` and `put hit=`, `arraylength: hit=`,
//   `aaload: hit=`, `aastore: hit=`, `door: static hit=`, `special hit=` and
//   `cast: hit=` (each in the hundreds of thousands); unset, no
//   `[site-cache]` line at all.
// * `CRATONVM_DBG_INVOKESTATS=1`: `[invokestats]` lines with a non-zero
//   `cache_hit=`; unset, none.
// * `CRATONVM_JIT_NO_DESCRIPTOR_FACTS=1 CRATONVM_DBG_FIELD_SITE=1`:
//   `door: static hit=0` and the decline census names "callee is
//   synchronized, native-backed, intercepted, or over-arity".
// * `CRATONVM_JIT_THRESHOLD=1` (JIT on): the same output lines.
// * `CRATONVM_JIT_NO_REF_ARRAY_FAST=1 CRATONVM_DBG_FIELD_SITE=1` (the
//   loop's hoisted `ref_array_fast_on`): `aaload: hit=0` and `aastore: hit=0`
//   while `arraylength: hit=` stays non-zero; unset, both in the hundreds of
//   thousands.
//
// Expected (HotSpot 25.0.3, `java L7W60HitGateCensus`, and `-Xint`):
//   cold field 7 9 16
//   cold array 3 3 b c
//   cold door 5 0 11 21 1 4
//   cold cast 1 true
//   loop field 300000 1200000
//   loop array 900000 300000 300000
//   loop door 1500000 300000 300000 300000 300000
//   loop cast 300000 300000
//   cold field 7 9 16
public class L7W60HitGateCensus {
    static class Base {
        int base() { return 10; }
    }

    static final class Node extends Base {
        int a;
        int b;
        Node(int a, int b) { this.a = a; this.b = b; }
        int sum() { return a + b; }
        private int twice() { return a * 2; }
        int callTwice() { return twice(); }
        int superBase() { return super.base() + 11; }
        @Override int base() { return 99; }
    }

    static final class Empty {}

    static int plus(int x, int y) { return x + y; }

    static void nothing() {}

    static String field() {
        Node n = new Node(7, 9);
        return "field " + n.a + " " + n.b + " " + n.sum();
    }

    static String array() {
        int[] ints = new int[3];
        String[] strs = {"a", "b", "c"};
        String[] copy = new String[3];
        for (int i = 0; i < strs.length; i++) {
            copy[i] = strs[i];
        }
        return "array " + ints.length + " " + copy.length + " " + copy[1] + " " + strs[2];
    }

    static String door() {
        Node n = new Node(2, 3);
        nothing();
        Empty e = new Empty();
        int ctor = e != null ? 1 : 0;
        return "door " + plus(2, 3) + " " + 0 + " " + plus(5, 6) + " " + n.superBase()
                + " " + ctor + " " + n.callTwice();
    }

    static String cast() {
        Object o = new Node(1, 0);
        int v = ((Node) o).a;
        boolean is = o instanceof Node;
        return "cast " + v + " " + is;
    }

    public static void main(String[] args) {
        System.out.println("cold " + field());
        System.out.println("cold " + array());
        System.out.println("cold " + door());
        System.out.println("cold " + cast());

        Node n = new Node(1, 0);
        long reads = 0;
        long writes = 0;
        for (int i = 0; i < 300_000; i++) {
            n.b = n.a;
            reads += n.sum();
            writes += n.a + n.b + n.a + n.b;
        }
        System.out.println("loop field " + reads / 2 + " " + writes);

        Object[] src = {"x", "y", "z"};
        Object[] dst = new Object[3];
        long lengths = 0;
        long loads = 0;
        long stores = 0;
        for (int i = 0; i < 300_000; i++) {
            lengths += src.length;
            Object v = src[i % 3];
            loads += v != null ? 1 : 0;
            dst[i % 3] = v;
            stores += dst[i % 3] == v ? 1 : 0;
        }
        System.out.println("loop array " + lengths + " " + loads + " " + stores);

        long statics = 0;
        long empties = 0;
        long privates = 0;
        long supers = 0;
        long ctors = 0;
        for (int i = 0; i < 300_000; i++) {
            statics += plus(2, 3);
            nothing();
            empties++;
            privates += n.callTwice() / 2;
            supers += n.superBase() / 21;
            ctors += new Empty() != null ? 1 : 0;
        }
        System.out.println("loop door " + statics + " " + empties + " " + privates + " "
                + supers + " " + ctors);

        Object o = new Node(1, 0);
        long casts = 0;
        long checks = 0;
        for (int i = 0; i < 300_000; i++) {
            casts += ((Node) o).a;
            checks += (o instanceof Node) ? 1 : 0;
        }
        System.out.println("loop cast " + casts + " " + checks);
        System.out.println("cold " + field());
    }
}
