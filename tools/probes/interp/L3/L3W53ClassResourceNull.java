// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 53, lane L3 -- `Class.getResource(null)` and
// `Class.getResourceAsStream(null)`: JDK 25's `Class.resolveName` dereferences
// the name first, an implicit NPE with HotSpot's helpful message and
// `resolveName` over the lookup on top. CratonVM serves both with natives: the
// stream door said `Class.getResourceAsStream: name is null` with the caller on
// top, the URL door answered `null`. Now both raise HotSpot's message, and the
// funnel's mark lists the two JDK frames (`stackwalker::served_raise_frames`,
// the implicit-NPE action). Each row prints the throwable's class and message,
// then the frames above the probe's first frame as `class.method`, then the
// first probe frame.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   stream-null  -- `getResourceAsStream(null)`;
//   url-null     -- `getResource(null)`;
//   stream-miss  -- `getResourceAsStream("nope.txt")`: null (control);
//   url-miss     -- `getResource("nope.txt")`: null (control);
//   stream-again -- `stream-null` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// stream-null: java.lang.NullPointerException Cannot invoke "String.startsWith(String)" because "name" is null [java.lang.Class.resolveName java.lang.Class.getResourceAsStream] L3W53ClassResourceNull.main
// url-null: java.lang.NullPointerException Cannot invoke "String.startsWith(String)" because "name" is null [java.lang.Class.resolveName java.lang.Class.getResource] L3W53ClassResourceNull.main
// stream-miss: null
// url-miss: null
// stream-again: java.lang.NullPointerException Cannot invoke "String.startsWith(String)" because "name" is null [java.lang.Class.resolveName java.lang.Class.getResourceAsStream] L3W53ClassResourceNull.main
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W53ClassResourceNull`.
public class L3W53ClassResourceNull {
    static void print(String name, Throwable t) {
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
        Class<?> c = L3W53ClassResourceNull.class;
        try { System.out.println("stream-null: " + c.getResourceAsStream(null)); } catch (Throwable t) { print("stream-null", t); }
        try { System.out.println("url-null: " + c.getResource(null)); } catch (Throwable t) { print("url-null", t); }
        try { System.out.println("stream-miss: " + c.getResourceAsStream("nope.txt")); } catch (Throwable t) { print("stream-miss", t); }
        try { System.out.println("url-miss: " + c.getResource("nope.txt")); } catch (Throwable t) { print("url-miss", t); }
        try { System.out.println("stream-again: " + c.getResourceAsStream(null)); } catch (Throwable t) { print("stream-again", t); }
    }
}
