// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Load each named class and report whether it verified. Used to run the VM's
// Pass-3 bytecode verifier over an *agent-generated* class file captured from
// a ByteBuddy/Mockito retransform: put the directory holding the woven
// `.class` first on the classpath and name the class here. `define_class` runs
// the same `verifier::verify_class` that `redefine_class` runs, so a rejection
// reproduces without needing the agent, the retransform, or the suite.

public class LoadAndVerifyProbe {
    public static void main(String[] args) {
        int bad = 0;
        for (String name : args) {
            try {
                Class<?> c = Class.forName(name, false, LoadAndVerifyProbe.class.getClassLoader());
                System.out.println("OK   " + name + " -> " + c);
            } catch (Throwable t) {
                bad++;
                System.out.println("FAIL " + name + " -> " + t);
                Throwable cause = t.getCause();
                while (cause != null) {
                    System.out.println("       caused by: " + cause);
                    cause = cause.getCause();
                }
            }
        }
        System.out.println(bad == 0 ? "PROBE-OK" : "PROBE-FAIL " + bad);
    }
}
