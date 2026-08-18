// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Torture case for the interpreter's resolved cast-site cache
// (`CRATONVM_JIT_NO_CAST_SITE_CACHE=1` opts out).
//
// The cache is direct-mapped on `(referencing class, cp index)` and stores a
// target `ClassId`. Every way it can be WRONG is silent: a wrong target id
// makes `instanceof` answer a plausible `true`/`false` and `checkcast` accept
// or reject the wrong type. Nothing throws to announce it. So every line below
// prints an exact expected value, chosen so that a mixed-up site produces a
// different one, and the whole output is compared between cache-ON and
// cache-OFF runs of the SAME binary.
//
// What each section targets:
//
//   1. same name, different referencing class — two classes each casting to
//      their own nested `Item`, so the cp index can collide while the
//      referencing class differs. That is the half of the key a tag check
//      might drop.
//   2. slot eviction — more distinct cast sites than the table has slots,
//      hammered in rotation so entries evict each other continuously.
//   3. interfaces, abstract supertypes and the negative answers — a hit is
//      only taken when `is_subclass_of` says yes, so the refusals must still
//      travel the full name-based path and agree.
//   4. arrays — excluded from the cache by construction; covariance and the
//      primitive-array refusals must be unaffected.
//   5. null — `checkcast` of null always succeeds, `instanceof` of null is
//      always false, neither should consult anything.
public final class CastSiteCacheTortureProbe {

    interface Marker {}
    interface Other {}

    static class Base {}
    static class Mid extends Base implements Marker {}
    static class Leaf extends Mid implements Other {}
    static class Unrelated {}

    static final class HolderA {
        static final class Item extends Base {
            int tag() { return 1; }
        }
        static String cast(Object o) {
            if (o instanceof Item) {
                return "A.Item:" + ((Item) o).tag();
            }
            return "A.not-Item";
        }
    }

    static final class HolderB {
        static final class Item extends Base {
            int tag() { return 2; }
        }
        static String cast(Object o) {
            if (o instanceof Item) {
                return "B.Item:" + ((Item) o).tag();
            }
            return "B.not-Item";
        }
    }

    // Section 2 needs many distinct cast sites. Each method is one site.
    static int siteRotation(Object[] objs, int rounds) {
        int acc = 0;
        for (int r = 0; r < rounds; r++) {
            for (Object o : objs) {
                if (o instanceof Base) acc += 1;
                if (o instanceof Mid) acc += 2;
                if (o instanceof Leaf) acc += 4;
                if (o instanceof Marker) acc += 8;
                if (o instanceof Other) acc += 16;
                if (o instanceof Unrelated) acc += 32;
                if (o instanceof HolderA.Item) acc += 64;
                if (o instanceof HolderB.Item) acc += 128;
                if (o instanceof String) acc += 256;
                if (o instanceof CharSequence) acc += 512;
                if (o instanceof Comparable) acc += 1024;
                if (o instanceof java.io.Serializable) acc += 2048;
                if (o instanceof Number) acc += 4096;
                if (o instanceof Integer) acc += 8192;
                if (o instanceof Object[]) acc += 16384;
                if (o instanceof int[]) acc += 32768;
            }
        }
        return acc;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200;

        Object[] objs = {
            new Base(), new Mid(), new Leaf(), new Unrelated(),
            new HolderA.Item(), new HolderB.Item(),
            "text", Integer.valueOf(7),
            new Object[1], new int[1], new Leaf[1],
        };

        // 1. Same simple name, different referencing class.
        StringBuilder sb = new StringBuilder();
        for (Object o : objs) {
            sb.append(HolderA.cast(o)).append('/').append(HolderB.cast(o)).append(' ');
        }
        System.out.println("names: " + sb.toString().trim());

        // 2. Eviction rotation — the accumulator is a checksum over every site.
        System.out.println("rotation: " + siteRotation(objs, rounds));

        // 3. Interfaces, supertypes and refusals, with the exact answers.
        for (Object o : objs) {
            System.out.println("shape " + o.getClass().getName()
                    + " base=" + (o instanceof Base)
                    + " mid=" + (o instanceof Mid)
                    + " leaf=" + (o instanceof Leaf)
                    + " marker=" + (o instanceof Marker)
                    + " other=" + (o instanceof Other)
                    + " unrel=" + (o instanceof Unrelated));
        }

        // 4. Arrays: covariance holds, primitive arrays refuse.
        Object leafArr = new Leaf[2];
        Object intArr = new int[2];
        System.out.println("arrays: " + (leafArr instanceof Base[]) + (leafArr instanceof Mid[])
                + (leafArr instanceof Object[]) + (leafArr instanceof int[])
                + " | " + (intArr instanceof Object[]) + (intArr instanceof int[]));
        System.out.println("array cast: " + ((Base[]) leafArr).length
                + " " + ((int[]) intArr).length);

        // 5. null behaviour.
        Object n = null;
        System.out.println("null: " + (n instanceof Base) + " " + ((Base) n));

        // 6. The refusals that must throw, with their exact classes.
        try {
            Object bad = new Base();
            Leaf l = (Leaf) bad;
            System.out.println("unreachable " + l);
        } catch (ClassCastException e) {
            System.out.println("cce base->leaf: " + e.getClass().getName());
        }
        try {
            Object bad = new HolderA.Item();
            HolderB.Item b = (HolderB.Item) bad;
            System.out.println("unreachable " + b);
        } catch (ClassCastException e) {
            System.out.println("cce A.Item->B.Item: " + e.getClass().getName());
        }
        try {
            Object bad = new int[1];
            Object[] oa = (Object[]) bad;
            System.out.println("unreachable " + oa.length);
        } catch (ClassCastException e) {
            System.out.println("cce int[]->Object[]: " + e.getClass().getName());
        }
    }
}
