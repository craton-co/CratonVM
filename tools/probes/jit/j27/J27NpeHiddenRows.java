// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 5 (lane NPEHIDDEN): the JEP 358 (helpful NPE) message of an NPE
// whose top frame is a HIDDEN method (`Method::is_hidden`: every method of a
// hidden class, and an `@jdk.internal.vm.annotation.Hidden` method of a boot /
// platform class such as the pre-generated `*$Holder` lambda forms or
// `VarHandleGuards.guard_*`). HotSpot 25's `JVM_GetExtendedNPEMessage` answers
// null for those (`java_lang_Throwable::get_top_method_and_bci` reads the
// backtrace's `trace_hidden_offset`, set by `fill_in_stack_trace` when it
// skipped a hidden top frame under `-XX:-ShowHiddenFrames`).
// Page: docs/feature-designs/jep358-helpful-npe.md
//
// Every throwing row has its own method, is WARMED with non-null inputs (so the
// JITs compile it with the throwing edge never taken), then throws THROWS
// times. `bad` counts later throws whose message differed from the first.
// THROWS is small on purpose: HotSpot's OmitStackTraceInFastThrow hands out
// preallocated, message-less NPEs once a compiled site has trapped often.
// `top=` is getStackTrace()[0] (class.method), after the hidden-frame drop.
//
// Compile with plain `javac` (no -g):
//   javac -d <dir> J27NpeHiddenRows.java
//   java -cp <dir> J27NpeHiddenRows > hs.txt
//   cratonvm --cp <dir> J27NpeHiddenRows | diff - hs.txt              (--real-jdk)
//   cratonvm --compatible --cp <dir> J27NpeHiddenRows | diff - hs.txt
//   also --nojit, and CRATONVM_NPE_HIDDEN_METHOD_RULE=0 (the old class-bit rule).
//
// Expected HotSpot 25 output (REASONED from the HotSpot sources named above,
// not measured; the host's HotSpot run is the authority):
//   fact mh-virtual-null-recv bad=0 msg=null top=J27NpeHiddenRows.mhVirtual
//   fact mh-filtered-null-recv bad=0 msg=null top=J27NpeHiddenRows.mhFiltered
//   fact mh-generic-null-recv bad=0 msg=null top=J27NpeHiddenRows.mhGeneric
//   fact mh-getter-null-holder bad=0 msg=null top=java.util.Objects.requireNonNull
//   fact vh-get-null-holder bad=0 msg=null top=java.util.Objects.requireNonNull
//   fact vh-set-null-holder bad=0 msg=null top=java.util.Objects.requireNonNull
//   fact user-getfield-null bad=0 msg=Cannot read field "v" because "J27NpeHiddenRows.NULL_BOX" is null top=J27NpeHiddenRows.userGetfield
// The next row is a MEASUREMENT, not a claim: a null MethodHandle's
// invokeExact traps inside the invoker lambda form (`Invokers.checkExactType`
// reads `mh.type()`), whose hidden-ness decides between `msg=null` and a
// `Cannot invoke "java.lang.invoke.MethodHandle.type()" ...` message.
//   fact mh-null-handle bad=0 msg=<host decides> top=<host decides>
// j27 wave 9 (lane W9EXC), measured on 25.0.3: default `java` and C1-only (the caller is compiled):
//   fact mh-null-handle bad=0 msg=Cannot invoke "java.lang.invoke.MethodHandle.type()" because "mh" is null top=java.lang.invoke.Invokers.checkExactType
// `java -Xint` / C2-only and `cratonvm --nojit`: the call-site NPE (msg=Cannot invoke
// "java.lang.invoke.MethodHandle.invokeExact(String)" because "<local1>" is null top=J27NpeHiddenRows.mhNullHandle).

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

class J27NhBox {
    int v;
}

public class J27NpeHiddenRows {
    static final int WARM = 20_000;
    static final int THROWS = 3;

    static final MethodHandle LEN;
    static final MethodHandle FILTERED;
    static final MethodHandle GETTER;
    static final VarHandle VH;
    static MethodHandle NULL_MH;
    static J27NhBox NULL_BOX;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            LEN = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
            FILTERED = MethodHandles.filterArguments(LEN, 0, MethodHandles.identity(String.class));
            GETTER = l.findGetter(J27NhBox.class, "v", int.class);
            VH = l.findVarHandle(J27NhBox.class, "v", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Row {
        int run(boolean nul) throws Throwable;
    }

    static int mhVirtual(boolean nul) throws Throwable {
        String s = nul ? null : "abc";
        return (int) LEN.invokeExact(s);
    }

    static int mhFiltered(boolean nul) throws Throwable {
        String s = nul ? null : "abcd";
        return (int) FILTERED.invokeExact(s);
    }

    static int mhGeneric(boolean nul) throws Throwable {
        Object o = nul ? null : "ab";
        return (int) LEN.invoke(o);
    }

    static final J27NhBox BOX = new J27NhBox();

    static int mhGetter(boolean nul) throws Throwable {
        J27NhBox b = nul ? null : BOX;
        return (int) GETTER.invokeExact(b);
    }

    static int vhGet(boolean nul) {
        J27NhBox b = nul ? null : BOX;
        return (int) VH.get(b);
    }

    static int vhSet(boolean nul) {
        J27NhBox b = nul ? null : BOX;
        VH.set(b, 7);
        return 0;
    }

    static MethodHandle WARM_MH = LEN;

    static int mhNullHandle(boolean nul) throws Throwable {
        MethodHandle mh = nul ? NULL_MH : WARM_MH;
        return (int) mh.invokeExact("xyz");
    }

    static int userGetfield(boolean nul) {
        if (nul) {
            return NULL_BOX.v;
        }
        return BOX.v;
    }

    static void row(String name, Row r) {
        long sink = 0;
        for (int i = 0; i < WARM; i++) {
            try {
                sink += r.run(false);
            } catch (Throwable t) {
                System.out.println("fact " + name + " warm-threw " + t);
                return;
            }
        }
        String first = null;
        String top = null;
        String kind = null;
        int bad = 0;
        for (int i = 0; i < THROWS; i++) {
            try {
                sink += r.run(true);
                System.out.println("fact " + name + " no-throw");
                return;
            } catch (Throwable t) {
                String msg = String.valueOf(t.getMessage());
                StackTraceElement[] st = t.getStackTrace();
                String tp = st.length == 0 ? "none" : st[0].getClassName() + "." + st[0].getMethodName();
                String k = t.getClass().getName();
                if (i == 0) {
                    first = msg;
                    top = tp;
                    kind = k;
                } else if (!msg.equals(first) || !tp.equals(top) || !k.equals(kind)) {
                    bad++;
                }
            }
        }
        String shown = "java.lang.NullPointerException".equals(kind) ? "" : kind + " ";
        if (sink == Long.MIN_VALUE) {
            System.out.println("unreachable");
        }
        System.out.println("fact " + name + " bad=" + bad + " " + shown + "msg=" + first + " top=" + top);
    }

    public static void main(String[] args) {
        row("mh-virtual-null-recv", J27NpeHiddenRows::mhVirtual);
        row("mh-filtered-null-recv", J27NpeHiddenRows::mhFiltered);
        row("mh-generic-null-recv", J27NpeHiddenRows::mhGeneric);
        row("mh-getter-null-holder", J27NpeHiddenRows::mhGetter);
        row("vh-get-null-holder", J27NpeHiddenRows::vhGet);
        row("vh-set-null-holder", J27NpeHiddenRows::vhSet);
        row("user-getfield-null", J27NpeHiddenRows::userGetfield);
        row("mh-null-handle", J27NpeHiddenRows::mhNullHandle);
    }
}
