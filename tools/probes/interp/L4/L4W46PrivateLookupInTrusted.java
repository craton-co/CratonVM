// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 46, lane L4 (review): `MethodHandles.privateLookupIn`
// with a TRUSTED caller (`Lookup.IMPL_LOOKUP`) returns `new
// Lookup(targetClass)` from the top of the method: FULL_POWER_MODES with
// ORIGINAL (95), no previous lookup class, and no module or
// `java.lang.invoke` check. CratonVM's native wrote 31 (ORIGINAL dropped).
// `Lookup.in` of the TRUSTED lookup is 95 too, and is not refused for a
// `java.lang.invoke` class (the control for `L4W46LookupInJavaLangInvoke`).
//
// Setup: `--add-opens java.base/java.lang.invoke=ALL-UNNAMED` (to read
// `IMPL_LOOKUP`).
//
// Run: javac -d out L4W46PrivateLookupInTrusted.java
//      cratonvm --java-home <jdk25> [--nojit] --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out L4W46PrivateLookupInTrusted
//
// Expected HotSpot 25 output (measured locally, with the flag):
//   pli-trusted: 95 null java.lang.String
//   pli-trusted-jli: 95 java.lang.invoke.MethodHandle
//   in-trusted-jli: 95 java.lang.invoke.MethodHandle
//
// On the base (55834015b), from the code: `pli-trusted` and
// `pli-trusted-jli` printed 31 (every mode). `--compatible` keeps 31 by
// design (the TRUSTED answer is `--jdk-only`'s).
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Field;

public class L4W46PrivateLookupInTrusted {
    public static void main(String[] args) throws Throwable {
        Field f = MethodHandles.Lookup.class.getDeclaredField("IMPL_LOOKUP");
        f.setAccessible(true);
        MethodHandles.Lookup impl = (MethodHandles.Lookup) f.get(null);
        MethodHandles.Lookup l = MethodHandles.privateLookupIn(String.class, impl);
        System.out.println("pli-trusted: " + l.lookupModes() + " " + l.previousLookupClass() + " " + l);
        MethodHandles.Lookup m = MethodHandles.privateLookupIn(MethodHandle.class, impl);
        System.out.println("pli-trusted-jli: " + m.lookupModes() + " " + m);
        MethodHandles.Lookup n = impl.in(MethodHandle.class);
        System.out.println("in-trusted-jli: " + n.lookupModes() + " " + n);
    }
}
