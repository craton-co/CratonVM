// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 51, lane L3 -- two reflection / security Bridges
// that ran over JDK 25 bytecode (`bridge-ran-over-bytecode` rows of
// docs/internal/fixed-bugs/interpreter-L5-bridges-still-run-over-jdk-bytecode-under-jdk-only-FIXED-20261007.md)
// and answered other than it:
//
//   * `Class.desiredAssertionStatus()` -- the JDK body asks the defining
//     loader's assertion table (`setDefaultAssertionStatus`,
//     `setClassAssertionStatus`, `setPackageAssertionStatus`,
//     `clearAssertionStatus`) before the `desiredAssertionStatus0` native; the
//     Bridge answered the VM-wide `-ea` default for every class;
//   * `AccessControlContext.<init>(ProtectionDomain[])` -- the JDK body keeps
//     the non-null domains (read by `equals`) and throws
//     `NullPointerException` for a null array; the Bridge kept nothing.
//
// Since wave 51 (`CRATONVM_REFLECTION_JDK_BODIES`,
// `lang_class::reflection_jdk_bodies`) neither is registered over a real JDK
// and the JDK bodies run, in both modes.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   app-class            -- this class (application loader, no -ea);
//   boot-class           -- `String` (boot loader: `desiredAssertionStatus0`);
//   loader-default-on    -- a class of a loader after `setDefaultAssertionStatus(true)`;
//   loader-class-off     -- then `setClassAssertionStatus("Twin", false)`;
//   loader-package-on    -- a class of package `p` after `setPackageAssertionStatus("p", true)`
//                           on a loader whose default is off;
//   loader-cleared       -- the first loader after `clearAssertionStatus()`;
//   acc-null             -- `new AccessControlContext(null)`;
//   acc-equals-same      -- two contexts over the same one domain;
//   acc-equals-empty     -- a context over one domain vs one over none;
//   acc-null-element     -- a context over `{null}` vs one over none;
//   app-class-again      -- `app-class` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// app-class: false
// boot-class: false
// loader-default-on: true
// loader-class-off: false
// loader-package-on: true
// loader-cleared: false
// acc-null: java.lang.NullPointerException
// acc-equals-same: true
// acc-equals-empty: false
// acc-null-element: true
// app-class-again: false
// HOTSPOT_EXPECTED_END
//
// CratonVM at the wave-51 base (measured, Windows, all four modes):
// `loader-default-on` and `loader-package-on` printed `false` (the Bridge
// ignores the loader's table), `acc-null` printed `ok`, `acc-equals-empty`
// printed `true` (neither context kept its domains). `loader-default-on` also
// needs the `ClassLoader.setDefaultAssertionStatus` Bridge to build the
// loader's assertion maps (the JDK body's `initializeJavaAssertionMaps()`),
// which it does under the same switch. Positive control:
// `CRATONVM_REFLECTION_JDK_BODIES=0` restores those answers.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W51ReflectionBridgesYield`.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.security.AccessControlContext;
import java.security.CodeSource;
import java.security.ProtectionDomain;

@SuppressWarnings("removal")
public class L3W51ReflectionBridgesYield {
    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            out = String.valueOf(call.run());
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(name + ": " + out);
    }

    static byte[] empty(String name) {
        return ClassFile.of().build(ClassDesc.of(name),
                cb -> cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER));
    }

    /** A loader that defines each of `names` itself (an empty class). */
    static ClassLoader loader(String tag, String... names) {
        return new ClassLoader(tag, L3W51ReflectionBridgesYield.class.getClassLoader()) {
            @Override
            protected Class<?> findClass(String n) throws ClassNotFoundException {
                for (String own : names) {
                    if (own.equals(n)) {
                        byte[] b = empty(n);
                        return defineClass(n, b, 0, b.length);
                    }
                }
                throw new ClassNotFoundException(n);
            }
        };
    }

    public static void main(String[] args) throws Throwable {
        row("app-class", () -> L3W51ReflectionBridgesYield.class.desiredAssertionStatus());
        row("boot-class", () -> String.class.desiredAssertionStatus());
        ClassLoader a = loader("a", "Twin");
        a.setDefaultAssertionStatus(true);
        Class<?> twin = Class.forName("Twin", false, a);
        row("loader-default-on", () -> twin.desiredAssertionStatus());
        a.setClassAssertionStatus("Twin", false);
        row("loader-class-off", () -> twin.desiredAssertionStatus());
        ClassLoader b = loader("b", "p.Pk");
        b.setPackageAssertionStatus("p", true);
        row("loader-package-on", () -> Class.forName("p.Pk", false, b).desiredAssertionStatus());
        a.clearAssertionStatus();
        row("loader-cleared", () -> twin.desiredAssertionStatus());

        ProtectionDomain pd = new ProtectionDomain(new CodeSource(null, (java.security.cert.Certificate[]) null), null);
        row("acc-null", () -> {
            new AccessControlContext(null);
            return "ok";
        });
        row("acc-equals-same", () -> new AccessControlContext(new ProtectionDomain[] {pd})
                .equals(new AccessControlContext(new ProtectionDomain[] {pd})));
        row("acc-equals-empty", () -> new AccessControlContext(new ProtectionDomain[] {pd})
                .equals(new AccessControlContext(new ProtectionDomain[0])));
        row("acc-null-element", () -> new AccessControlContext(new ProtectionDomain[] {null})
                .equals(new AccessControlContext(new ProtectionDomain[0])));
        row("app-class-again", () -> L3W51ReflectionBridgesYield.class.desiredAssertionStatus());
    }
}
