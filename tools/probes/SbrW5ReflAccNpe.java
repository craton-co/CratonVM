// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;

/**
 * SBR wave 5, lane {@code reflacc}: the helpful-NPE text of {@code Field}'s
 * value accessors on a null receiver, cold and hot. Each row prints the first
 * message and then every iteration whose message differs from it (with the
 * iteration number), so a JIT that attributes the NPE to the wrong expression
 * once the accessor is compiled shows up as a changed line.
 *
 * <p>HotSpot 25 (interpreter and C1) says {@code Cannot invoke
 * "Object.getClass()" because "obj" is null} for the non-{@code override}
 * accessors (the NPE is {@code checkAccess}'s {@code obj.getClass()}); C2
 * drops the message ({@code null}) once it compiles the throwing path. What
 * must never appear is a message about a different expression.
 */
public class SbrW5ReflAccNpe {
    public static class H {
        public int i = 1;
        public long l = 2;
        public Object o = "x";
    }

    interface Access {
        void run(Field f) throws Exception;
    }

    static void row(String name, Field f, Access a, int iters) {
        String first = null;
        int changed = 0;
        for (int k = 0; k < iters; k++) {
            String m;
            try {
                a.run(f);
                m = "no exception";
            } catch (Exception e) {
                m = e.getClass().getName() + ": " + e.getMessage();
            }
            if (k == 0) {
                first = m;
                System.out.println("row " + name + " = " + m);
            } else if (!m.equals(first)) {
                if (changed++ < 3) System.out.println("  iter " + k + " " + name + " = " + m);
            }
        }
        System.out.println("  " + name + " changed " + changed);
    }

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        Field fi = H.class.getField("i");
        Field fl = H.class.getField("l");
        Field fo = H.class.getField("o");
        Field fa = H.class.getField("i");
        fa.setAccessible(true);
        row("get", fo, f -> f.get(null), iters);
        row("getInt", fi, f -> f.getInt(null), iters);
        row("getLong", fl, f -> f.getLong(null), iters);
        row("set", fo, f -> f.set(null, "y"), iters);
        row("setInt", fi, f -> f.setInt(null, 5), iters);
        row("getInt override", fa, f -> f.getInt(null), iters);
        row("setInt override", fa, f -> f.setInt(null, 5), iters);
        row("get wrong receiver", fi, f -> f.get("str"), iters);
        row("setInt wrong receiver", fa, f -> f.setInt("str", 1), iters);
        row("set wrong value", fo, f -> H.class.getField("i").set(new H(), "str"), iters);
    }
}
