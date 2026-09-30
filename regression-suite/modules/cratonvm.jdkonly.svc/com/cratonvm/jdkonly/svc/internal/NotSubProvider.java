package com.cratonvm.jdkonly.svc.internal;

import com.cratonvm.jdkonly.svc.Unsub;

/**
 * THIS SOURCE IS NOT WHAT RUNS -- read regression-suite/modules-overlay/ first.
 *
 * javac REFUSES a {@code provides ... with X} clause whose provider neither
 * implements the service nor declares a conforming {@code provider()} factory
 * ("the service implementation type must be a subtype of the service interface
 * type"), so the illegal shape cannot be written in module-info.java form at
 * all. That refusal is precisely why {@code ServiceLoader.loadProvider} carries
 * the rule as a RUNTIME gate: the module it defends against is one assembled
 * without javac, or compiled against a different version of the service type.
 *
 * Same two-source device as {@code WrongFactory}: this file exists only to
 * satisfy javac while {@code module-info.java} is compiled, and
 * {@code regression-suite/modules-overlay/.../NotSubProvider.java} -- which
 * implements NOTHING -- is compiled straight over the resulting class file in a
 * second javac pass (regression-suite/run.sh {@code compile_modules}). The
 * class file that reaches the VM is the OVERLAY's. Editing this file alone
 * changes nothing that runs.
 */
public final class NotSubProvider implements Unsub {
    public NotSubProvider() {
    }

    @Override
    public String id() {
        return "never-reached";
    }
}
