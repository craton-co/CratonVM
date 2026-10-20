// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 125, lane L5 --
// `i38-L5-proposal-the-initiating-record-replaces-the-memo-under-jdk-only`:
// a user loader that overrides `loadClass` (so it is not "transparent" for
// JDK-global names and the VM asks it for one) and delegates a `javax.` name
// to its parent, the application loader. Its class resolves
// `javax.net.SocketFactory`. HotSpot asks the loader once, its parent chain
// answers the JDK class, and the loader becomes an initiating loader of it:
// `findLoadedClass("javax.net.SocketFactory")` answers the class afterwards,
// and a second class of the same loader resolving the same name does not ask
// again (a dictionary hit). CratonVM kept that answer in the capped memo
// (`drive_loader_for_global_name`'s refusal memo), which `findLoadedClass`
// does not read; `CRATONVM_GLOBAL_NAME_REFUSAL_RECORD=1` files it in the
// initiating record when the checked door's reason is `BaseMiss`.
// Wave 133 (lane L5): this loader's drive is not a refusal -- it ANSWERS the
// class (`Ok(Ok(id))`) -- and the successful drive's record writer dropped
// every JDK-global name but `java/`, so `find=null` with the switch on or off.
// `CRATONVM_GLOBAL_NAME_INITIATING_RECORD` (default on, `=0` off) records it.
//
// Run: javac -d . L5W125GlobalNameRefusalRecord.java
//      java -cp . L5W125GlobalNameRefusalRecord   (HotSpot)
//      cratonvm --jdk-only|--compatible -cp . L5W125GlobalNameRefusalRecord
//      CRATONVM_GLOBAL_NAME_REFUSAL_RECORD=1 cratonvm --jdk-only -cp . L5W125GlobalNameRefusalRecord
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// before find=null asks=0
// first-resolve same=true
// after-first find=same asks=1
// second-resolve same=true asks=1
// after-second find=same
// HOTSPOT_EXPECTED_END

import java.io.InputStream;
import java.lang.reflect.Method;

public class L5W125GlobalNameRefusalRecord {
    static final String GLOBAL = "javax.net.SocketFactory";

    public static class Ref1 {
        public static Object get() {
            return javax.net.SocketFactory.class;
        }
    }

    public static class Ref2 {
        public static Object get() {
            return javax.net.SocketFactory.class;
        }
    }

    static final class CountingLoader extends ClassLoader {
        int asks;

        CountingLoader(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(GLOBAL)) {
                asks++;
            }
            if (name.startsWith("L5W125GlobalNameRefusalRecord$Ref")) {
                synchronized (getClassLoadingLock(name)) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        String res = name.replace('.', '/') + ".class";
                        try (InputStream in = getParent().getResourceAsStream(res)) {
                            byte[] b = in.readAllBytes();
                            c = defineClass(name, b, 0, b.length);
                        } catch (java.io.IOException e) {
                            throw new ClassNotFoundException(name, e);
                        }
                    }
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }

        Class<?> peek(String name) {
            return findLoadedClass(name);
        }
    }

    static Object call(ClassLoader l, String cls) throws Exception {
        Method m = l.loadClass(cls).getMethod("get");
        return m.invoke(null);
    }

    static String find(CountingLoader l, Class<?> expected) {
        Class<?> c = l.peek(GLOBAL);
        return c == null ? "null" : (c == expected ? "same" : "other");
    }

    public static void main(String[] args) throws Exception {
        Class<?> jdk = javax.net.SocketFactory.class;
        CountingLoader l = new CountingLoader(L5W125GlobalNameRefusalRecord.class.getClassLoader());
        System.out.println("before find=" + find(l, jdk) + " asks=" + l.asks);
        Object r1 = call(l, "L5W125GlobalNameRefusalRecord$Ref1");
        System.out.println("first-resolve same=" + (r1 == jdk));
        System.out.println("after-first find=" + find(l, jdk) + " asks=" + l.asks);
        Object r2 = call(l, "L5W125GlobalNameRefusalRecord$Ref2");
        System.out.println("second-resolve same=" + (r2 == jdk) + " asks=" + l.asks);
        System.out.println("after-second find=" + find(l, jdk));
    }
}
