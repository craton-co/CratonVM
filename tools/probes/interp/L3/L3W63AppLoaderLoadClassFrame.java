// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 63, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-an-application-loader-miss-lists-an-appclassloader-loadclass-frame-RETIRED-20261007.md
// (OPEN, both modes). JDK 25's `ClassLoaders$AppClassLoader` declares no
// `loadClass` (the security-manager override went with JEP 486), so HotSpot
// lists no `AppClassLoader.loadClass` frame in an application-loader miss;
// the wave-63 host run of `L3W63DoorInFlightKey` listed one. Row `declares`
// tells whether the class bytes this VM loaded declare the method (real
// bytes: the frame is captured as run; stand-in: the frame is rebuilt), the
// other rows are the miss reached three ways. Rows print the throwable's
// frames up to the probe's own lambda, no lines.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// declares: false
// loader-loadClass: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | L3W63AppLoaderLoadClassFrame.lambda$main$1
// forName1: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | java.lang.Class.forName | L3W63AppLoaderLoadClassFrame.lambda$main$2
// forName3: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass | java.lang.ClassLoader.loadClass | java.lang.Class.forName0(N) | java.lang.Class.forName | java.lang.Class.forName | L3W63AppLoaderLoadClassFrame.lambda$main$3
// HOTSPOT_EXPECTED_END
//
// The `forName1` / `forName3` rows are direct calls: the public method and
// its `@CallerSensitiveAdapter` overload both run (two `forName` frames),
// unlike a method handle, which binds the adapter (one frame; see
// `interpreter-L3-a-handle-to-a-caller-sensitive-method-runs-the-public-method-not-its-adapter-RETIRED-20261007.md`).
// If the orchestrator's oracle shows one `forName` frame in `forName3`, the
// JDK's 3-argument body calls `forName0` itself; correct the block from the
// real run.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W63AppLoaderLoadClassFrame`.
import java.lang.reflect.Method;

public class L3W63AppLoaderLoadClassFrame {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            StringBuilder b = new StringBuilder(name + ": " + t.getClass().getName() + " ");
            String sep = "";
            for (StackTraceElement e : t.getStackTrace()) {
                b.append(sep).append(e.getClassName()).append('.').append(e.getMethodName())
                        .append(e.isNativeMethod() ? "(N)" : "");
                sep = " | ";
                if (e.getMethodName().startsWith("lambda$")) {
                    break;
                }
            }
            System.out.println(b);
        }
    }

    public static void main(String[] args) throws Throwable {
        ClassLoader app = L3W63AppLoaderLoadClassFrame.class.getClassLoader();
        row("declares", () -> {
            for (Method m : app.getClass().getDeclaredMethods()) {
                if (m.getName().equals("loadClass")) {
                    return true;
                }
            }
            return false;
        });
        row("loader-loadClass", () -> app.loadClass("no.Such"));
        row("forName1", () -> Class.forName("no.Such"));
        row("forName3", () -> Class.forName("no.Such", false, app));
    }
}
