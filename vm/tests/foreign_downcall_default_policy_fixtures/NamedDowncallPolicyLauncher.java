// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
public final class NamedDowncallPolicyLauncher {
    public static void main(String[] args) throws Throwable {
        Class<?> probe = Class.forName("ffm.probe.ForeignDowncallPolicyProbe");
        if (!"ffm.probe".equals(probe.getModule().getName())) throw new AssertionError("named caller");
        probe.getMethod("main", String[].class).invoke(null, (Object) new String[0]);
    }
}
