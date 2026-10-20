// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Method;
import java.util.function.Supplier;

/**
 * {@code Method.invoke} on a method looked up on a lambda's own runtime class
 * ({@code lam.getClass().getMethod(..)}) must run the lambda's body.
 *
 * <p>Found by the gc JNI retirement (2026-10-10): JNA reaches an
 * interface-mapped callback this way. A lambda proxy lives in the VM's lambda
 * proxy table, not the class manager, so the reflective door's "is the
 * declaring class initialized" test answered no, and the initialization it
 * then started refused the id: the VM aborted with
 * {@code internal error: prepare_class: class 2147483648 not found}, in both
 * {@code --jdk-only} and {@code --compatible}. Fixed in
 * {@code vm/src/vm/vm_util.rs} ({@code is_class_initialized_via_manager}: a
 * registered lambda proxy is initialized, as HotSpot's spun class is).
 *
 * <p>HotSpot 25 prints, and exits 0:
 * <pre>
 *   own-class: PASS
 *   declared: PASS
 *   capturing: PASS
 *   interface: PASS
 *   PASS all 4
 * </pre>
 */
public class LambdaOwnClassReflectInvokeProbe {
    public interface Cmp { int invoke(Object a, Object b); }

    static int pass, fail;

    static void check(String name, boolean ok, String detail) {
        if (ok) { pass++; System.out.println(name + ": PASS"); }
        else { fail++; System.out.println(name + ": FAIL " + detail); }
    }

    public static void main(String[] args) throws Exception {
        int[] calls = {0};
        Cmp lam = (x, y) -> { calls[0]++; return ((String) x).compareTo((String) y); };

        Method own = lam.getClass().getMethod("invoke", Object.class, Object.class);
        own.setAccessible(true);
        Object r = own.invoke(lam, "a", "b");
        check("own-class", calls[0] == 1 && Integer.valueOf(-1).equals(r), "calls=" + calls[0] + " r=" + r);

        Method declared = lam.getClass().getDeclaredMethod("invoke", Object.class, Object.class);
        r = declared.invoke(lam, "b", "a");
        check("declared", calls[0] == 2 && Integer.valueOf(1).equals(r), "calls=" + calls[0] + " r=" + r);

        String captured = "cap";
        Supplier<String> sup = () -> captured + calls[0];
        Method get = sup.getClass().getMethod("get");
        r = get.invoke(sup);
        check("capturing", "cap2".equals(r), "r=" + r);

        Method viaIface = Cmp.class.getMethod("invoke", Object.class, Object.class);
        r = viaIface.invoke(lam, "c", "c");
        check("interface", calls[0] == 3 && Integer.valueOf(0).equals(r), "calls=" + calls[0] + " r=" + r);

        if (fail == 0) System.out.println("PASS all " + pass);
        else { System.out.println("FAIL " + fail + " of " + (pass + fail)); System.exit(1); }
    }
}
