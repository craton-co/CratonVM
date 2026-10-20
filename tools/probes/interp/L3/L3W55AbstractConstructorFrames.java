// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L3 -- the frames on top of the
// `InstantiationException` `Constructor.newInstance` raises for an abstract
// class's constructor. JDK 25's `ReflectionFactory.newConstructorAccessor`
// hands out an `InstantiationExceptionConstructorAccessorImpl(null)`, whose
// `newInstance` throws the message-less exception under
// `Constructor.newInstanceWithCaller` and `Constructor.newInstance`. The VM's
// native builds the throwable itself inside the reflective call, whose frames
// listed the call's own accessor on top
// (`jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance`, both
// modes, measured on a debug build before the fix).
//
// Each row prints the throwable's class, message and cause, the frames above
// the probe's first frame as `class.method`, then the first probe frame's
// class.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   abstract        -- `Abs.class.getConstructor().newInstance()`;
//   abstract-args   -- an abstract class's one-argument constructor;
//   jdk-abstract    -- `java.io.InputStream`'s constructor;
//   abstract-again  -- `abstract` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// abstract: java.lang.InstantiationException null null [jdk.internal.reflect.InstantiationExceptionConstructorAccessorImpl.newInstance java.lang.reflect.Constructor.newInstanceWithCaller java.lang.reflect.Constructor.newInstance] L3W55AbstractConstructorFrames
// abstract-args: java.lang.InstantiationException null null [jdk.internal.reflect.InstantiationExceptionConstructorAccessorImpl.newInstance java.lang.reflect.Constructor.newInstanceWithCaller java.lang.reflect.Constructor.newInstance] L3W55AbstractConstructorFrames
// jdk-abstract: java.lang.InstantiationException null null [jdk.internal.reflect.InstantiationExceptionConstructorAccessorImpl.newInstance java.lang.reflect.Constructor.newInstanceWithCaller java.lang.reflect.Constructor.newInstance] L3W55AbstractConstructorFrames
// abstract-again: java.lang.InstantiationException null null [jdk.internal.reflect.InstantiationExceptionConstructorAccessorImpl.newInstance java.lang.reflect.Constructor.newInstanceWithCaller java.lang.reflect.Constructor.newInstance] L3W55AbstractConstructorFrames
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W55AbstractConstructorFrames`.
import java.util.concurrent.Callable;

public class L3W55AbstractConstructorFrames {
    public abstract static class Abs {
        public Abs() {}

        public Abs(String s) {}
    }

    static void raise(String name, Callable<Object> c) {
        try {
            System.out.println(name + ": " + c.call());
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int i = 0;
            while (i < st.length && !st[i].getClassName().startsWith("L3W55")) {
                jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
                i++;
            }
            String first = i < st.length ? st[i].getClassName() : "-";
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " " + t.getCause()
                    + " [" + jdk + "] " + first);
        }
    }

    public static void main(String[] args) throws Exception {
        raise("abstract", () -> Abs.class.getConstructor().newInstance());
        raise("abstract-args", () -> Abs.class.getConstructor(String.class).newInstance("x"));
        raise("jdk-abstract", () -> {
            var c = java.io.InputStream.class.getConstructor();
            return c.newInstance();
        });
        raise("abstract-again", () -> Abs.class.getConstructor().newInstance());
    }
}
