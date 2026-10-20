// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L3: `Method.toString()` and
// `Constructor.toString()` of members of HIDDEN classes, and of members whose
// parameter, return or array types are hidden. `Class.getTypeName()` of a
// hidden class is its `getName()`, `p.Host/0x<hex>`: the `/0x` tail keeps its
// slash. CratonVM's `Method.toString` native turned every `/` into `.`
// (`p.Host.0x...`); that native serves only where the JDK body does not run,
// so the rows are expected SAME in both modes either way. They pin the
// native's answer too.
//
// The hidden class is this probe's `Template` nested class, defined again
// with `Lookup.defineHiddenClass` from its own bytes. Methods are sorted by
// name, then by `toString`; addresses are folded to `/0xADDR`.
//
// Run: javac -d . L3W116HiddenClassMethodToString.java
//      java -cp . L3W116HiddenClassMethodToString                  (HotSpot reference)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L3W116HiddenClassMethodToString
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// hidden isHidden=true
// hidden name: L3W116HiddenClassMethodToString$Template/0xADDR typeName: L3W116HiddenClassMethodToString$Template/0xADDR
// hidden method: public L3W116HiddenClassMethodToString$Template[] L3W116HiddenClassMethodToString$Template/0xADDR.many(L3W116HiddenClassMethodToString$Template[][]) throws java.lang.IllegalStateException
// hidden method: public java.lang.String L3W116HiddenClassMethodToString$Template/0xADDR.plain()
// hidden method: public static L3W116HiddenClassMethodToString$Template L3W116HiddenClassMethodToString$Template/0xADDR.self(L3W116HiddenClassMethodToString$Template)
// hidden ctor: public L3W116HiddenClassMethodToString$Template/0xADDR()
// proxy isHidden=true
// proxy method: public java.lang.Object L3W116HiddenClassMethodToString$$Lambda/0xADDR.get()
// HOTSPOT_EXPECTED_END

import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;

public class L3W116HiddenClassMethodToString {
    public static class Template {
        public Template() {}

        public static Template self(Template t) {
            return t;
        }

        public Template[] many(Template[][] grid) throws IllegalStateException {
            return grid[0];
        }

        public String plain() {
            return "plain";
        }
    }

    static String fold(String s) {
        return s.replaceAll("/0x[0-9a-fA-F]+", "/0xADDR");
    }

    static void print(String label, Class<?> c) {
        List<String> rows = new ArrayList<>();
        for (Method m : c.getDeclaredMethods()) {
            rows.add(m.getName() + " " + fold(m.toString()));
        }
        rows.sort(null);
        System.out.println(label + " name: " + fold(c.getName()) + " typeName: " + fold(c.getTypeName()));
        for (String r : rows) {
            System.out.println(label + " method: " + r.substring(r.indexOf(' ') + 1));
        }
        for (Constructor<?> k : c.getDeclaredConstructors()) {
            System.out.println(label + " ctor: " + fold(k.toString()));
        }
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes;
        try (InputStream in = L3W116HiddenClassMethodToString.class
                .getResourceAsStream("L3W116HiddenClassMethodToString$Template.class")) {
            bytes = in.readAllBytes();
        }
        Class<?> hidden = MethodHandles.lookup().defineHiddenClass(bytes, true).lookupClass();
        System.out.println("hidden isHidden=" + hidden.isHidden());
        print("hidden", hidden);
        Supplier<String> lambda = () -> "x";
        Class<?> proxy = lambda.getClass();
        System.out.println("proxy isHidden=" + proxy.isHidden());
        List<String> rows = new ArrayList<>();
        for (Method m : proxy.getDeclaredMethods()) {
            rows.add(fold(m.toString()));
        }
        rows.sort(null);
        for (String r : rows) {
            System.out.println("proxy method: " + r);
        }
    }
}
