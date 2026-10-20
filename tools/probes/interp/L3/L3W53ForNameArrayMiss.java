// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 53, lane L3 -- `Class.forName` of an array of a
// missing class: HotSpot loads the element through the loader, so the
// `ClassNotFoundException` is the loader's miss, with its `loadClass` frames
// above `Class.forName0`
// (docs/internal/fixed-bugs/interpreter-L3-forname-of-an-array-of-a-missing-class-lists-no-loader-frames-FIXED-20261017.md).
// Each row prints the throwable's class and message, then the frames above
// the probe's first frame as `class.method` (no line numbers), then the first
// probe frame.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   array          -- `Class.forName("[Lnope;")`;
//   array-loader   -- `Class.forName("[Lnope;", false, <app loader>)`;
//   array-2d       -- `Class.forName("[[Lnope;")`;
//   array-java     -- `Class.forName("[Ljava.lang.Nope;")`;
//   plain          -- `Class.forName("Nope")`, the plain miss (control);
//   array-again    -- `array` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// array: java.lang.ClassNotFoundException nope [jdk.internal.loader.BuiltinClassLoader.loadClass java.lang.ClassLoader.loadClass java.lang.Class.forName0 java.lang.Class.forName java.lang.Class.forName] L3W53ForNameArrayMiss.main
// array-loader: java.lang.ClassNotFoundException nope [jdk.internal.loader.BuiltinClassLoader.loadClass java.lang.ClassLoader.loadClass java.lang.Class.forName0 java.lang.Class.forName] L3W53ForNameArrayMiss.main
// array-2d: java.lang.ClassNotFoundException nope [jdk.internal.loader.BuiltinClassLoader.loadClass java.lang.ClassLoader.loadClass java.lang.Class.forName0 java.lang.Class.forName java.lang.Class.forName] L3W53ForNameArrayMiss.main
// array-java: java.lang.ClassNotFoundException java.lang.Nope [jdk.internal.loader.BuiltinClassLoader.loadClass java.lang.ClassLoader.loadClass java.lang.Class.forName0 java.lang.Class.forName java.lang.Class.forName] L3W53ForNameArrayMiss.main
// plain: java.lang.ClassNotFoundException Nope [jdk.internal.loader.BuiltinClassLoader.loadClass java.lang.ClassLoader.loadClass java.lang.Class.forName0 java.lang.Class.forName java.lang.Class.forName] L3W53ForNameArrayMiss.main
// array-again: java.lang.ClassNotFoundException nope [jdk.internal.loader.BuiltinClassLoader.loadClass java.lang.ClassLoader.loadClass java.lang.Class.forName0 java.lang.Class.forName java.lang.Class.forName] L3W53ForNameArrayMiss.main
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W53ForNameArrayMiss`.
public class L3W53ForNameArrayMiss {
    static void print(String name, Throwable t) {
        if (t == null) {
            System.out.println(name + ": no exception");
            return;
        }
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder jdk = new StringBuilder();
        int i = 0;
        while (i < st.length && !st[i].getClassName().startsWith("L3W53")) {
            jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
            i++;
        }
        String first = i < st.length ? st[i].getClassName() + "." + st[i].getMethodName() : "-";
        System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
    }

    public static void main(String[] args) {
        ClassLoader app = L3W53ForNameArrayMiss.class.getClassLoader();
        Throwable t;
        t = null; try { Class.forName("[Lnope;"); } catch (Throwable e) { t = e; } print("array", t);
        t = null; try { Class.forName("[Lnope;", false, app); } catch (Throwable e) { t = e; } print("array-loader", t);
        t = null; try { Class.forName("[[Lnope;"); } catch (Throwable e) { t = e; } print("array-2d", t);
        t = null; try { Class.forName("[Ljava.lang.Nope;"); } catch (Throwable e) { t = e; } print("array-java", t);
        t = null; try { Class.forName("Nope"); } catch (Throwable e) { t = e; } print("plain", t);
        t = null; try { Class.forName("[Lnope;"); } catch (Throwable e) { t = e; } print("array-again", t);
    }
}
