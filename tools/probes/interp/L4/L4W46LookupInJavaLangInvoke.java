// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 46, lane L4 (review): JDK 25 `Lookup.newLookup`
// refuses a lookup class of `java.lang.invoke`
// (`checkUnprivilegedlookupClass`: `IllegalArgumentException: illegal
// lookupClass: <Class.toString()>`) for every lookup `Lookup.in` and
// `MethodHandles.privateLookupIn` make, except a TRUSTED caller's and
// `in(<its own lookup class>)`. CratonVM's `Lookup.in` native answered a
// lookup for every such row.
// `privateLookupIn` reaches the check only through `--add-opens
// java.base/java.lang.invoke=ALL-UNNAMED` (not a row here).
//
// Run: javac -d out L4W46LookupInJavaLangInvoke.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W46LookupInJavaLangInvoke
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   in-mh: java.lang.IllegalArgumentException: illegal lookupClass: class java.lang.invoke.MethodHandle
//   in-lookup: java.lang.IllegalArgumentException: illegal lookupClass: class java.lang.invoke.MethodHandles$Lookup
//   in-info: java.lang.IllegalArgumentException: illegal lookupClass: interface java.lang.invoke.MethodHandleInfo
//   public-in-mh: java.lang.IllegalArgumentException: illegal lookupClass: class java.lang.invoke.MethodHandle
//   dropped-in-mh: java.lang.IllegalArgumentException: illegal lookupClass: class java.lang.invoke.MethodHandle
//   in-string: java.lang.String/L4W46LookupInJavaLangInvoke/public
//
// `--compatible` keeps its old answers by design (the check is `--jdk-only`
// only): the five refused rows print a lookup's `toString()` there.
//
// On the base (55834015b), from the code, every mode: the five refused rows
// printed a lookup; no native spelled `illegal lookupClass`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.util.concurrent.Callable;

public class L4W46LookupInJavaLangInvoke {
    static void row(String name, Callable<Object> c) {
        String out;
        try {
            out = String.valueOf(c.call());
        } catch (Throwable t) {
            out = t.toString();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) {
        row("in-mh", () -> MethodHandles.lookup().in(MethodHandle.class));
        row("in-lookup", () -> MethodHandles.lookup().in(MethodHandles.Lookup.class));
        row("in-info", () -> MethodHandles.lookup().in(MethodHandleInfo.class));
        row("public-in-mh", () -> MethodHandles.publicLookup().in(MethodHandle.class));
        row("dropped-in-mh", () -> MethodHandles.lookup()
                .dropLookupMode(MethodHandles.Lookup.PUBLIC).in(MethodHandle.class));
        row("in-string", () -> MethodHandles.lookup().in(String.class));
    }
}
