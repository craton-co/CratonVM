// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Item 5 of docs/internal/fixed-bugs/interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007.md
// (2026-10-07): a native agent's JNI `FindClass` from its `VMInit` callback
// (a thread with no Java frame) goes through the system class loader on
// HotSpot, which then lists the class among the classes it initiated. The
// agent (`I47JniFindClassWithoutAFrame.c`) finds
// `java.util.concurrent.Exchanger`; nothing else loads it. A control row
// names `java.util.concurrent.Phaser`, which nobody finds.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3, default flags and -Xint)
// vminit-findclass=true control=false
// HOTSPOT_EXPECTED_END
// Without the Java agent: `no agent`.
//
// SETUP:
//   Windows: cl /LD /I "%JAVA_HOME%\include" /I "%JAVA_HOME%\include\win32"
//               I47JniFindClassWithoutAFrame.c /Fe:findclass.dll
//   Linux:   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux"
//               -o libfindclass.so I47JniFindClassWithoutAFrame.c
//   a jar whose manifest has `Premain-Class: I47JniFindClassWithoutAFrame$Agent`
//   containing I47JniFindClassWithoutAFrame*.class, then
//   java|cratonvm [--compatible] -agentpath:<the library> -javaagent:probe.jar
//       -cp probe.jar I47JniFindClassWithoutAFrame
import java.lang.instrument.Instrumentation;

public class I47JniFindClassWithoutAFrame {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static boolean holds(Class<?>[] classes, String name) {
        for (Class<?> c : classes) {
            if (c.getName().equals(name)) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        Class<?>[] list = i.getInitiatedClasses(ClassLoader.getSystemClassLoader());
        System.out.println("vminit-findclass=" + holds(list, "java.util.concurrent.Exchanger")
                + " control=" + holds(list, "java.util.concurrent.Phaser"));
    }
}
