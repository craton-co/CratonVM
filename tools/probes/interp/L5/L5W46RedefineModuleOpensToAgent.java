// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 46, lane L5: `Instrumentation.redefineModule`
// opening `java.base/java.lang` to the agent's (the application loader's)
// unnamed module, then `MethodHandles.privateLookupIn` -- JaCoCo 0.8.15's
// `InjectedClassRuntime` sequence. Wave 45's `privateLookupIn` module check
// (`classloader::private_lookup_in_module_refusal`, `Module.isOpen(pn,
// caller)`) refused it under `--jdk-only`: the opening, recorded through
// `Module.implAddOpens(pn, unnamedModule)`, was stored as granting nobody
// (`UNRESOLVED_TARGET_MODULE`), and JaCoCo's premain died with
// `InvocationTargetException` (`[ACCESS-DBG] privateLookupIn refused: module
// java.base does not open java.lang to unnamed module`).
//
// Rows:
//   before       privateLookupIn(Object.class, lookup()) before the opening
//   open-before  java.base.isOpen("java.lang", <this unnamed module>)
//   open-after   the same after redefineModule
//   open-all     java.base.isOpen("java.lang") after (unqualified: false)
//   open-other   isOpen("java.lang", <another loader's unnamed module>) after
//   exported     java.base.isExported("jdk.internal.misc", <this unnamed
//                module>) after (a package the call did not touch: false)
//   lookup       privateLookupIn(Object.class, lookup()) after: its class
//   access       String.class.getDeclaredField("value").setAccessible(true)
//                after (the package is open to this class's module)
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     before=java.lang.IllegalAccessException
//     open-before=false
//     open-after=true
//     open-all=false
//     open-other=false
//     exported=false
//     lookup=java.lang.Object
//     access=ok
// CratonVM on the base `55834015b` (`--jdk-only`, from the code): the edge
// grants nobody, so `open-after=false`, `lookup=java.lang.IllegalAccessException`
// and `access=java.lang.reflect.InaccessibleObjectException`.
// `--compatible` output differs by design (it asks no module question in
// `privateLookupIn` and records an opening to an unnamed module as granting
// nobody, as before wave 46); measured on the wave-46 host run, it differs
// from HotSpot in `before=java.lang.Object` and `open-after=false`.
// Positive control: `CRATONVM_DBG=access` prints no `privateLookupIn refused`
// line for the `lookup` row (it prints one for `before`).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L5W46RedefineModuleOpensToAgent$Agent
// containing L5W46RedefineModuleOpensToAgent*.class, then
//     java|cratonvm [--nojit] -javaagent:probe.jar -cp probe.jar L5W46RedefineModuleOpensToAgent
// Without the agent both VMs print "no agent".
import java.lang.instrument.Instrumentation;
import java.lang.invoke.MethodHandles;
import java.util.Map;
import java.util.Set;

public class L5W46RedefineModuleOpensToAgent {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName());
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        Module base = Object.class.getModule();
        Module mine = L5W46RedefineModuleOpensToAgent.class.getModule();
        Module other = new ClassLoader(null) { }.getUnnamedModule();
        row("before", () -> MethodHandles.privateLookupIn(Object.class, MethodHandles.lookup())
                .lookupClass().getName());
        row("open-before", () -> base.isOpen("java.lang", mine));
        i.redefineModule(base, Set.of(), Map.of(), Map.of("java.lang", Set.of(mine)),
                Set.of(), Map.of());
        row("open-after", () -> base.isOpen("java.lang", mine));
        row("open-all", () -> base.isOpen("java.lang"));
        row("open-other", () -> base.isOpen("java.lang", other));
        row("exported", () -> base.isExported("jdk.internal.misc", mine));
        row("lookup", () -> MethodHandles.privateLookupIn(Object.class, MethodHandles.lookup())
                .lookupClass().getName());
        row("access", () -> {
            String.class.getDeclaredField("value").setAccessible(true);
            return "ok";
        });
    }
}
