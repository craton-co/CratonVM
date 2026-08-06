// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Mock every class named in a list file through Mockito's inline mock maker.
//
// One `mock()` is one ByteBuddy retransform, and every retransform hands a
// freshly woven class file to `redefine_class` — i.e. to the Pass-3 bytecode
// verifier. A single mock (see `InfinispanMockRetransformProbe`) proves only
// that one woven shape verifies; this sweeps hundreds so that a verifier that
// over-counts operand-stack depth for some construct ByteBuddy emits has a
// real chance of being caught, rather than being ruled out from one green
// class.
//
// Usage: MockManyProbe <list-file>
//   list-file: one fully-qualified class name per line, `#` comments allowed.
//
// Output is one line per class (OK / SKIP / FAIL) plus a summary. A retransform
// the VM rejects does NOT throw here — Mockito logs and carries on — so the
// run's real verdict is the absence of `UnsupportedClassRedefinitionError` on
// stderr and of files under `CRATONVM_DBG_REDEFINE_DUMP`.

import java.io.BufferedReader;
import java.io.FileReader;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.List;

public class MockManyProbe {
    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("usage: MockManyProbe <list-file>");
            return;
        }
        List<String> names = new ArrayList<>();
        try (BufferedReader r = new BufferedReader(new FileReader(args[0]))) {
            String line;
            while ((line = r.readLine()) != null) {
                line = line.trim();
                if (!line.isEmpty() && !line.startsWith("#")) {
                    names.add(line);
                }
            }
        }

        Class<?> mockito = Class.forName("org.mockito.Mockito");
        Method mock = mockito.getMethod("mock", Class.class);

        int ok = 0, skip = 0, fail = 0;
        for (String name : names) {
            Class<?> cls;
            try {
                cls = Class.forName(name, false, MockManyProbe.class.getClassLoader());
            } catch (Throwable t) {
                skip++;
                System.out.println("SKIP " + name + " (not loadable: " + t.getClass().getSimpleName() + ")");
                continue;
            }
            int m = cls.getModifiers();
            if (cls.isInterface() || cls.isEnum() || cls.isAnnotation() || cls.isArray()
                    || Modifier.isAbstract(m) || !Modifier.isPublic(m)) {
                // Interfaces and abstract types are proxied, not retransformed:
                // they never reach `redefine_class`, so they teach nothing here.
                skip++;
                System.out.println("SKIP " + name + " (not a concrete public class)");
                continue;
            }
            try {
                Object o = mock.invoke(null, cls);
                ok++;
                System.out.println("OK   " + name + " -> " + (o == null ? "null" : "mock"));
            } catch (Throwable t) {
                fail++;
                Throwable c = t.getCause() != null ? t.getCause() : t;
                System.out.println("FAIL " + name + " -> " + c);
            }
        }
        System.out.println("SUMMARY ok=" + ok + " skip=" + skip + " fail=" + fail);
        System.out.println(fail == 0 ? "PROBE-OK" : "PROBE-FAIL");
    }
}
