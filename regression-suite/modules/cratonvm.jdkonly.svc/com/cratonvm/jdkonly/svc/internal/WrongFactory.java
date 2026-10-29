package com.cratonvm.jdkonly.svc.internal;

import com.cratonvm.jdkonly.svc.Rejected;

/**
 * THIS SOURCE IS NOT WHAT RUNS -- read regression-suite/modules-overlay/ first.
 *
 * javac REFUSES to compile a `provides ... with X` clause whose provider
 * neither implements the service nor declares a `provider()` returning a
 * subtype of it ("the \"provider\" method return type must be a subtype of the
 * service interface type"), so the illegal shape cannot be written in
 * module-info.java form at all. That is precisely why
 * `ServiceLoader.loadProvider` carries the check as a RUNTIME gate: the module
 * it has to defend against is one that was assembled without javac, or against
 * a different version of the service type.
 *
 * The suite reproduces that separately-compiled shape the only honest way:
 * this file exists to satisfy javac while `module-info.java` is compiled, and
 * `regression-suite/modules-overlay/.../WrongFactory.java` -- whose
 * `provider()` returns `Object` -- is compiled straight over the resulting
 * class file in a second javac pass (regression-suite/run.sh `compile_modules`).
 * The class file that reaches the VM is the OVERLAY's. Editing this file alone
 * changes nothing that runs.
 */
public final class WrongFactory {
    private WrongFactory() {
    }

    public static Rejected provider() {
        return () -> "never-reached";
    }
}
