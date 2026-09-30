// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 13, lane L5: a user type's own
// `setDefaultAssertionStatus(boolean)` must run.
//
// CratonVM intercepts `setDefaultAssertionStatus(Z)V` by NAME and descriptor
// and answers it with the `ClassLoader` no-op (a Surefire workaround): in
// `vm_exec::invoke_or_native` (the JIT's by-name tail, which
// `site_name_is_special_cased` sends every such call to) and in the
// interpreter's `intercept_classloader_set_default_assertion_status` (the
// cached virtual doors). Before wave 13 neither asked whether the receiver
// was a ClassLoader, so calls to the user methods below were skipped and the
// counters stay below 300000 (which calls depends on the tier and on the
// inline-cache state of each site).
//
// Run with the default settings and with --nojit; diff against HotSpot 25,
// which prints exactly:
//
//   instance calls=300000 last=false
//   static calls=300000 last=false
//   loader ok
public class UserSetDefaultAssertionStatus {
    static final class Toggle {
        int calls;
        boolean last;

        public void setDefaultAssertionStatus(boolean enabled) {
            calls++;
            last = enabled;
        }
    }

    static final class StaticToggle {
        static int calls;
        static boolean last;

        static void setDefaultAssertionStatus(boolean enabled) {
            calls++;
            last = enabled;
        }
    }

    static void driveInstance(Toggle t, int n) {
        for (int i = 0; i < n; i++) {
            t.setDefaultAssertionStatus((i & 1) == 0);
        }
    }

    static void driveStatic(int n) {
        for (int i = 0; i < n; i++) {
            StaticToggle.setDefaultAssertionStatus((i & 1) == 0);
        }
    }

    public static void main(String[] args) {
        Toggle t = new Toggle();
        // Many short calls so the drivers compile as ordinary methods, not
        // only through on-stack replacement.
        for (int round = 0; round < 300; round++) {
            driveInstance(t, 1000);
            driveStatic(1000);
        }
        System.out.println("instance calls=" + t.calls + " last=" + t.last);
        System.out.println("static calls=" + StaticToggle.calls + " last=" + StaticToggle.last);
        // The real receiver keeps working (the intercept's own purpose).
        ClassLoader loader = UserSetDefaultAssertionStatus.class.getClassLoader();
        loader.setDefaultAssertionStatus(false);
        System.out.println("loader ok");
    }
}
