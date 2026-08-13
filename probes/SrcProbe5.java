// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Is `java/util/logging/LogRecord` the REAL class file, or the synthetic mint
// from classloading/src/class_manager.rs `synthetic_stub_ctor_methods`?
//
// PRINT IT, do not infer. The mint declares 24 methods, every one
// PUBLIC|NATIVE and body-less, and declares no `needToInferCaller`. The real
// JDK 25 class file declares ~40 methods, NONE of them native, and carries
// `needToInferCaller`. Either answer settles W7-56: if the class is minted,
// `inferCaller()` does not exist to be called and retiring the natives cannot
// help; if it is real, the natives are still winning dispatch and the
// retirement is not reaching them.
//
//   java -cp probes SrcProbe5
//   cratonvm --java-home <jdk> --jdk-only -cp probes SrcProbe5
//   cratonvm --java-home <jdk> --real-jdk -cp probes SrcProbe5
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;

public class SrcProbe5 {

    static void report(Class<?> c) {
        System.out.println("CLASS " + c.getName()
                + " module=" + (c.getModule() == null ? "?" : c.getModule().getName())
                + " loader=" + c.getClassLoader()
                + " methods=" + c.getDeclaredMethods().length
                + " fields=" + c.getDeclaredFields().length);
        int nativeCount = 0;
        for (Method m : c.getDeclaredMethods()) {
            if (Modifier.isNative(m.getModifiers())) {
                nativeCount++;
            }
        }
        System.out.println("  nativeDeclaredMethods=" + nativeCount);
        for (String n : new String[] {"getSourceClassName", "setSourceClassName",
                                      "getSourceMethodName", "getMessage"}) {
            try {
                Method m = c.getDeclaredMethod(n, n.startsWith("set")
                        ? new Class<?>[] {String.class} : new Class<?>[0]);
                System.out.println("  " + n + " native=" + Modifier.isNative(m.getModifiers()));
            } catch (NoSuchMethodException e) {
                System.out.println("  " + n + " ABSENT");
            }
        }
        for (String f : new String[] {"needToInferCaller", "sourceClassName", "message"}) {
            try {
                c.getDeclaredField(f);
                System.out.println("  field " + f + " PRESENT");
            } catch (NoSuchFieldException e) {
                System.out.println("  field " + f + " ABSENT");
            }
        }
    }

    /** Does `inferCaller` exist at all? Only the real class file declares it. */
    static void inferCallerExists() {
        try {
            Method m = LogRecord.class.getDeclaredMethod("inferCaller");
            System.out.println("inferCaller DECLARED native=" + Modifier.isNative(m.getModifiers()));
        } catch (NoSuchMethodException e) {
            System.out.println("inferCaller ABSENT -- the class is not the real one");
        }
        try {
            Class<?> cf = Class.forName("java.util.logging.LogRecord$CallerFinder");
            System.out.println("CallerFinder LOADABLE " + cf.getName());
        } catch (Throwable t) {
            System.out.println("CallerFinder UNLOADABLE " + t.getClass().getName());
        }
    }

    static final class Probe extends Handler {
        @Override
        public void publish(LogRecord r) {
            System.out.println("B during-publish class=" + r.getSourceClassName()
                    + " method=" + r.getSourceMethodName());
        }

        @Override
        public void flush() {}

        @Override
        public void close() {}
    }

    public static void main(String[] args) {
        report(LogRecord.class);
        report(Logger.class);
        inferCallerExists();
        Logger log = Logger.getLogger("srcprobe5.one");
        log.setUseParentHandlers(false);
        log.addHandler(new Probe());
        log.warning("MARK5");
        System.out.println("DONE SrcProbe5");
    }
}
