// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Which mock maker actually served each `mock()` — inline or subclass?
//
// Mockito's inline mock maker retransforms the target class, so the mock's
// runtime class IS the target class. When the retransform is unavailable or
// fails, Mockito silently falls back to the subclass maker, whose mock is a
// generated `Target$MockitoMock$…` subclass. Both answer `mock()` successfully,
// so a probe that only asks "did mock() throw?" cannot tell them apart — and
// the subclass fallback is a real fidelity gap (it cannot intercept final
// methods, final classes, or instances that already exist).
//
// A `final` target is the exception that makes the fallback visible: there is
// nothing to subclass, so `mock()` throws instead of quietly degrading. That is
// why `org.infinispan.query.remote.client.impl.QueryRequest` (public final) was
// the only class in a 1901-class sweep to fail on CratonVM while HotSpot mocked
// it — the interesting question is not that one class but whether every other
// mock in the process is silently a subclass proxy.
//
// Usage: MockMakerKindProbe <class-name>...
// Output per class: INLINE / SUBCLASS / FAIL, plus whether the target is final.

import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

public class MockMakerKindProbe {
    public static void main(String[] args) throws Exception {
        Class<?> mockito = Class.forName("org.mockito.Mockito");
        Method mock = mockito.getMethod("mock", Class.class);

        int inline = 0, subclass = 0, failed = 0;
        for (String name : args) {
            Class<?> cls;
            try {
                cls = Class.forName(name);
            } catch (Throwable t) {
                System.out.println("SKIP     " + name + " (" + t.getClass().getSimpleName() + ")");
                continue;
            }
            String shape = Modifier.isFinal(cls.getModifiers()) ? "final" : "non-final";
            try {
                Object m = mock.invoke(null, cls);
                boolean isInline = m.getClass() == cls;
                if (isInline) {
                    inline++;
                } else {
                    subclass++;
                }
                System.out.println((isInline ? "INLINE   " : "SUBCLASS ") + name
                        + " [" + shape + "] -> " + m.getClass().getName());
            } catch (Throwable t) {
                failed++;
                Throwable c = t.getCause() != null ? t.getCause() : t;
                System.out.println("FAIL     " + name + " [" + shape + "] -> " + c);
                for (Throwable e = c; e != null && e.getCause() != e; e = e.getCause()) {
                    System.out.println("           " + e.getClass().getName() + ": " + e.getMessage());
                }
            }
        }
        System.out.println("SUMMARY inline=" + inline + " subclass=" + subclass + " fail=" + failed);
        System.out.println(subclass == 0 && failed == 0 ? "PROBE-OK" : "PROBE-FAIL");
    }
}
