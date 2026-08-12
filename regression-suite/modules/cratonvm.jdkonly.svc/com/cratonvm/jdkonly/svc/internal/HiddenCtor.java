package com.cratonvm.jdkonly.svc.internal;

import com.cratonvm.jdkonly.svc.Ctored;

/**
 * THIS SOURCE IS NOT WHAT RUNS -- read regression-suite/modules-overlay/ first.
 *
 * javac refuses a {@code provides ... with X} clause whose provider has no
 * public no-arg constructor ("the service implementation ... does not have a
 * public no-argument constructor"), so, exactly as for {@code WrongFactory} and
 * {@code NotSubProvider}, the illegal shape has to be produced by a second
 * javac pass with no module context. This file satisfies javac while
 * {@code module-info.java} is compiled; the class file the VM loads is
 * {@code regression-suite/modules-overlay/.../HiddenCtor.java}, whose no-arg
 * constructor is PRIVATE.
 */
public final class HiddenCtor implements Ctored {
    public HiddenCtor() {
    }

    @Override
    public String id() {
        return "never-reached";
    }
}
