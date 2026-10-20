// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L5: by-name lookups made by JDK natives
// on behalf of a class a child-first user loader defined, the shapes the
// `CRATONVM_DBG=access` `CALLER-DRIVE` census attributes (step 3 of
// `i53-L5-proposal-ctx-load-class-takes-an-explicit-initiating-loader`, "Wave 116").
// Every lookup names a class only the child loader has (its own copy of
// `Peer`), so each answer says WHICH loader resolved it. stdout must be
// HotSpot's in both modes; with `CRATONVM_DBG=access`, stderr's
// `[ACCESS-DBG] CALLER-DRIVE` rows list the sites that drove the calling
// frame's loader (none is the HotSpot-like answer: each of these JDK entry
// points is given its loader explicitly or by `Reflection.getCallerClass`).
//
// Run: javac -d . L5W116CallerDriveCensusShapes.java
//      java -cp . L5W116CallerDriveCensusShapes                        (HotSpot reference)
//      CRATONVM_DBG=access cratonvm --jdk-only -cp . L5W116CallerDriveCensusShapes 2>access.txt
//      CRATONVM_DBG=access cratonvm --compatible -cp . L5W116CallerDriveCensusShapes 2>access.txt
//      grep CALLER-DRIVE access.txt
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// forname-one-arg child
// forname-three-arg child
// array-of-peer child
// method-type child
// proxy-of-peer child
// field-type child
// method-return-type child
// HOTSPOT_EXPECTED_END

import java.io.InputStream;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.lang.reflect.Proxy;
import java.util.function.Supplier;

public class L5W116CallerDriveCensusShapes {
    public interface Peer {
        String id();
    }

    /** Defined by the child loader; asks the JDK to resolve `Peer` by name. */
    public static class Asker implements Supplier<String> {
        public Peer field;

        public Peer make() {
            return null;
        }

        static String who(Class<?> c, ClassLoader mine) {
            return c.getClassLoader() == mine ? "child" : "other:" + c.getClassLoader();
        }

        public String get() {
            ClassLoader mine = Asker.class.getClassLoader();
            String peer = "L5W116CallerDriveCensusShapes$Peer";
            StringBuilder sb = new StringBuilder();
            try {
                sb.append("forname-one-arg ").append(who(Class.forName(peer), mine)).append('\n');
                sb.append("forname-three-arg ").append(who(Class.forName(peer, false, mine), mine)).append('\n');
                Object arr = Array.newInstance(Class.forName(peer), 1);
                sb.append("array-of-peer ").append(who(arr.getClass().getComponentType(), mine)).append('\n');
                MethodType mt = MethodType.fromMethodDescriptorString("(L" + peer + ";)V", mine);
                sb.append("method-type ").append(who(mt.parameterType(0), mine)).append('\n');
                Object p = Proxy.newProxyInstance(mine, new Class<?>[] {Class.forName(peer)}, (o, m, a) -> "x");
                sb.append("proxy-of-peer ").append(who(p.getClass().getInterfaces()[0], mine)).append('\n');
                sb.append("field-type ").append(who(Asker.class.getField("field").getType(), mine)).append('\n');
                sb.append("method-return-type ").append(who(Asker.class.getMethod("make").getReturnType(), mine));
            } catch (Exception e) {
                sb.append("error ").append(e);
            }
            return sb.toString();
        }
    }

    static final class ChildFirst extends ClassLoader {
        ChildFirst() {
            super(L5W116CallerDriveCensusShapes.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (name.startsWith("L5W116CallerDriveCensusShapes$")) {
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
                return super.loadClass(name, resolve);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        ChildFirst child = new ChildFirst();
        @SuppressWarnings("unchecked")
        Supplier<String> asker = (Supplier<String>) child
                .loadClass("L5W116CallerDriveCensusShapes$Asker")
                .getConstructor()
                .newInstance();
        System.out.println(asker.get());
    }
}
