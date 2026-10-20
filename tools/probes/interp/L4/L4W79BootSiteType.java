// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L4 -- page
// `docs/internal/fixed-bugs/i72-L4-a-bootstrap-callers-converting-invoke-resolves-its-site-through-the-system-loader-FIXED-20261011.md`
// (switch `CRATONVM_MH_SITE_TYPE_BOOT_LOADER`, default ON since 2026-10-11).
//
// A BOOT-loader class (`L4W79BootSiteTypeBootSide`, appended with
// `-Xbootclasspath/a`) calls `invoke` on a real method handle at a call site
// whose return type names a class only the APPLICATION loader can see
// (`L4W79BootSiteTypeAppOnly`). HotSpot links the `invokehandle` site with the
// caller's (boot) loader and fails: `NoClassDefFoundError`. `--jdk-only`
// resolved the site's `MethodType` with `fromMethodDescriptorString(desc,
// null)`, whose `null` is the SYSTEM loader, which finds the class: the call
// ran. With `CRATONVM_MH_SITE_TYPE_BOOT_LOADER=1` the site resolves with the
// boot loader (`MethodType.fromDescriptor(desc, null)`) and fails too. Wave 81
// (lane L4): the factory's `TypeNotPresentException` is rewritten to the link's
// `NoClassDefFoundError: <internal name>` (no cause for the boot loader, as
// HotSpot; `lang_invoke.rs` `mh_site_type_link_error`), so the switch arm reads
// SAME and the default arm DIFF (`ran: true`). `control` (an app-loader caller)
// must run in every configuration.
//
// `inlined-user-site` (2026-10-11, the flip's gate): a JDK loop
// (`IntStream.range(..).forEach`, boot-loader code) runs a USER lambda whose
// converting `invoke` site names the application-only class, often enough
// for the JIT to compile the loop and inline the lambda. A compiled frame
// whose inline chain is not exact reports its owner (a boot class) as the
// caller; the site must still resolve with the user's loader, as HotSpot's.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// control: app-only
// boot-loader: null
// boot-site: java.lang.NoClassDefFoundError: L4W79BootSiteTypeAppOnly
// inlined-user-site: 30000
// HOTSPOT_EXPECTED_END
//
// SETUP (the boot side must be on the boot class path ONLY):
//   javac -d . L4W79BootSiteType.java
//   mkdir -p boot && mv L4W79BootSiteTypeBootSide.class boot/
//   java|cratonvm [--jdk-only] -Xbootclasspath/a:boot -cp . L4W79BootSiteType
//   CRATONVM_MH_SITE_TYPE_BOOT_LOADER=1 cratonvm --jdk-only -Xbootclasspath/a:boot -cp . L4W79BootSiteType
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W79BootSiteType {
    public static Object make(Object o) {
        return new L4W79BootSiteTypeAppOnly();
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        // Wave 96 (lane L3): an identity hash (`AppClassLoader@1affbebc`, what
        // `boot-loader` prints when the battery runs without the SETUP below)
        // differs per VM run; print the stable form.
        out = out.replaceAll("@[0-9a-f]+\\b", "");
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        // `(Object)Object`: every call below is a converting `invoke`.
        MethodHandle mh = MethodHandles.lookup().findStatic(L4W79BootSiteType.class, "make",
                MethodType.methodType(Object.class, Object.class));
        row("control", () -> {
            L4W79BootSiteTypeAppOnly r = (L4W79BootSiteTypeAppOnly) mh.invoke((Object) "x");
            return r.toString();
        });
        // The boot side is package-private in ANOTHER runtime package (the boot
        // loader's), so it is reached reflectively, with `setAccessible`.
        Class<?> boot = Class.forName("L4W79BootSiteTypeBootSide");
        row("boot-loader", boot::getClassLoader);
        row("boot-site", () -> {
            Method call = boot.getDeclaredMethod("call", MethodHandle.class);
            call.setAccessible(true);
            try {
                return call.invoke(null, mh);
            } catch (InvocationTargetException e) {
                throw e.getCause();
            }
        });
        row("inlined-user-site", () -> {
            int[] ran = new int[1];
            java.util.stream.IntStream.range(0, 30000).forEach(i -> {
                try {
                    L4W79BootSiteTypeAppOnly r = (L4W79BootSiteTypeAppOnly) mh.invoke((Object) "x");
                    if (r != null) {
                        ran[0]++;
                    }
                } catch (Throwable t) {
                    throw new RuntimeException(t);
                }
            });
            return ran[0];
        });
    }
}

/** Application-loader only. */
class L4W79BootSiteTypeAppOnly {
    @Override
    public String toString() {
        return "app-only";
    }
}

/** Moved to `boot/` by the setup: defined by the boot loader. */
class L4W79BootSiteTypeBootSide {
    static String call(MethodHandle mh) throws Throwable {
        // A signature-polymorphic call: the cast is the site's return type
        // `(Object)L4W79BootSiteTypeAppOnly;`, linked with this class's loader.
        Object r = (L4W79BootSiteTypeAppOnly) mh.invoke((Object) "x");
        return "ran: " + (r != null);
    }
}
