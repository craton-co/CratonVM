// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 107, lane L3 (hunt; fixed in the same wave, see
// docs/internal/fixed-bugs/interpreter-i106-L3-constant-pool-member-resolvers-unregistered-FIXED-20261016.md
// "Wave 107"): a `Lookup.defineHiddenClass` class's own `this_class` entry is
// held RESOLVED to the hidden class by HotSpot, so `ConstantPool.getClassAt`
// on it, and `getMethodAt` on a reference to one of its own methods, answer
// the hidden class. CratonVM resolved the entry's NAME through the loader,
// which finds (or loads) the ordinary class of that name -- another class.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; derived from ClassFileParser's hidden
// class this_class patch and jvm.cpp's get_method_at_helper)
// hidden-is-hidden: true
// own-class-entry-is-hidden: true
// own-method-ref-declaring-is-hidden: true
// own-method-ref: static int L3W107HiddenClassOwnPoolH.twice(int)
// HOTSPOT_EXPECTED_END
//
// SETUP: `javac L3W107HiddenClassOwnPool.java`, then
//   java|cratonvm [--compatible] [--nojit] --add-exports java.base/jdk.internal.access=ALL-UNNAMED
//     --add-exports java.base/jdk.internal.reflect=ALL-UNNAMED -cp . L3W107HiddenClassOwnPool
// (the `/0x...` suffix of the hidden name is never printed: the own-method-ref
// row prints the declaring class through `Method.toString`, which names it
// with the suffix on both VMs -- so the row strips it).
import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W107HiddenClassOwnPool {
    static Object cp;
    static Class<?> cpc;

    static Object call(String name, int index) throws Throwable {
        try {
            return cpc.getMethod(name, int.class).invoke(cp, index);
        } catch (InvocationTargetException e) {
            throw e.getCause();
        }
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes;
        try (InputStream in = L3W107HiddenClassOwnPool.class
                .getResourceAsStream("L3W107HiddenClassOwnPoolH.class")) {
            bytes = in.readAllBytes();
        }
        Class<?> hidden = MethodHandles.lookup().defineHiddenClass(bytes, true).lookupClass();
        System.out.println("hidden-is-hidden: " + hidden.isHidden());
        hidden.getDeclaredMethod("run").invoke(null);

        Class<?> ss = Class.forName("jdk.internal.access.SharedSecrets");
        Object jla = ss.getMethod("getJavaLangAccess").invoke(null);
        cp = Class.forName("jdk.internal.access.JavaLangAccess")
                .getMethod("getConstantPool", Class.class).invoke(jla, hidden);
        cpc = Class.forName("jdk.internal.reflect.ConstantPool");
        int size = (Integer) cpc.getMethod("getSize").invoke(cp);
        boolean ownClass = false;
        String ownRef = "absent";
        boolean ownDeclaring = false;
        for (int i = 1; i < size; i++) {
            String tag = String.valueOf(call("getTagAt", i));
            if (tag.equals("CLASS")) {
                try {
                    if (call("getClassAt", i) == hidden) {
                        ownClass = true;
                    }
                } catch (Throwable t) {
                    // Another class that does not resolve: not this row's question.
                }
            } else if (tag.equals("METHODREF")) {
                String[] info = (String[]) call("getMemberRefInfoAt", i);
                if (info[1].equals("twice")) {
                    try {
                        Method m = (Method) call("getMethodAt", i);
                        ownDeclaring = m.getDeclaringClass() == hidden;
                        ownRef = m.toString().replaceAll("/0x[0-9a-fA-F]+", "");
                    } catch (Throwable t) {
                        ownRef = t.getClass().getName() + " " + t.getMessage();
                    }
                }
            }
        }
        System.out.println("own-class-entry-is-hidden: " + ownClass);
        System.out.println("own-method-ref-declaring-is-hidden: " + ownDeclaring);
        System.out.println("own-method-ref: " + ownRef);
    }
}

class L3W107HiddenClassOwnPoolH {
    static int twice(int x) {
        return x * 2;
    }

    public static void run() {
        if (twice(2) != 4) {
            throw new AssertionError();
        }
    }
}
