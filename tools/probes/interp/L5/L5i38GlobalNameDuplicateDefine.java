// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// `i38-L5` closure (2026-11-02) -- the two JDK-global-name rows the initiating
// record still left to the capped memo (`CRATONVM_GLOBAL_NAME_REFUSAL_RECORD`,
// `CRATONVM_LOADER_DICTIONARY_ONE_DOOR`).
//
// A loader that overrides `loadClass` (so the VM asks it) answers
// `javax.net.ServerSocketFactory` from its parent chain: it becomes an
// initiating loader of the JDK class (JVMS 5.3.1; a dictionary row).
//   1. `Class.forName(name, false, l)` makes that row WITHOUT a memo row, so a
//      class of `l` that later resolves the name by `ldc` must not ask `l`
//      again: HotSpot finds the dictionary entry (`asks` stays 1).
//   2. `l.defineClass(name, bytes)` after that is the duplicate-definition
//      `LinkageError` (SystemDictionary::check_constraints); `javax.*` is not
//      a prohibited package, so the define reaches that check. A fresh loader
//      that initiated nothing defines the bytes (control).
//
// Run: javac -d out L5i38GlobalNameDuplicateDefine.java
//      java -cp out L5i38GlobalNameDuplicateDefine                        (HotSpot)
//      cratonvm --real-jdk|--compatible -cp out L5i38GlobalNameDuplicateDefine
//   switches (each a separate run, both modes):
//      CRATONVM_GLOBAL_NAME_REFUSAL_RECORD=1   rows 'resolve' and 'define-after-initiating'
//      CRATONVM_LOADER_DICTIONARY_ONE_DOOR=1   row 'resolve'
//   Default VM (both off): 'resolve ... asks=2' and 'define-after-initiating=defined'
//   are the open gap this probe measures.
//
// Measured on HotSpot 25.0.3 with -Xshare:off on 2026-11-03. Only loader
// identity hashes are normalized; the class kind and module clause stay visible.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured)
// forname same=true asks=1
// find after-forname=same
// resolve same=true asks=1
// define-after-initiating=java.lang.LinkageError message=loader L5i38GlobalNameDuplicateDefine$AskLoader @ID attempted duplicate abstract class definition for javax.net.ServerSocketFactory. (javax.net.ServerSocketFactory is in unnamed module of loader L5i38GlobalNameDuplicateDefine$AskLoader @ID, parent loader 'app')
// define-fresh=true
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.io.InputStream;

public class L5i38GlobalNameDuplicateDefine {
    static final String TARGET = "javax.net.ServerSocketFactory";
    static final String OWN_PREFIX = "L5i38GlobalNameDuplicateDefine$";

    /** Resolves TARGET by {@code ldc} from a class the probe loader defines. */
    public static class Ref {
        public static Object get() {
            return javax.net.ServerSocketFactory.class;
        }
    }

    static final class AskLoader extends ClassLoader {
        final ClassLoader source;
        int asks;

        AskLoader(ClassLoader parent, ClassLoader source) {
            super(parent);
            this.source = source;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(TARGET)) {
                asks++;
            }
            if (name.startsWith(OWN_PREFIX)) {
                synchronized (getClassLoadingLock(name)) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        String res = name.replace('.', '/') + ".class";
                        try (InputStream in = source.getResourceAsStream(res)) {
                            if (in == null) {
                                throw new ClassNotFoundException(name);
                            }
                            byte[] b = in.readAllBytes();
                            c = defineClass(name, b, 0, b.length);
                        } catch (IOException e) {
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

        Class<?> define(String name, byte[] bytes) {
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    static String tag(Class<?> found, Class<?> expected) {
        return found == null ? "null" : (found == expected ? "same" : "other");
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = L5i38GlobalNameDuplicateDefine.class.getClassLoader();
        Class<?> jdk = javax.net.ServerSocketFactory.class;
        AskLoader l = new AskLoader(app, app);

        Class<?> viaForName = Class.forName(TARGET, false, l);
        System.out.println("forname same=" + (viaForName == jdk) + " asks=" + l.asks);
        System.out.println("find after-forname=" + tag(l.peek(TARGET), jdk));

        Class<?> ref = Class.forName(OWN_PREFIX + "Ref", true, l);
        Object got = ref.getMethod("get").invoke(null);
        System.out.println("resolve same=" + (got == jdk) + " asks=" + l.asks);

        byte[] bytes;
        try (InputStream in = app.getResourceAsStream("javax/net/ServerSocketFactory.class")) {
            if (in == null) {
                System.out.println("bytes=missing");
                return;
            }
            bytes = in.readAllBytes();
        }
        try {
            l.define(TARGET, bytes);
            System.out.println("define-after-initiating=defined");
        } catch (LinkageError e) {
            String m = String.valueOf(e.getMessage());
            System.out.println("define-after-initiating=" + e.getClass().getName()
                    + " message=" + m.replaceAll("@[0-9a-fA-F]+", "@ID"));
        } catch (Throwable e) {
            System.out.println("define-after-initiating=" + e.getClass().getName());
        }

        AskLoader fresh = new AskLoader(app, app);
        try {
            Class<?> c = fresh.define(TARGET, bytes);
            System.out.println("define-fresh=" + (c.getClassLoader() == fresh));
        } catch (Throwable e) {
            System.out.println("define-fresh=" + e.getClass().getName());
        }
    }
}
