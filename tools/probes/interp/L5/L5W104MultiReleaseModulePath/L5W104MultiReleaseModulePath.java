// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 104, lane L5 -- see setup.sh for the build, the
// run commands and the expected output.

import java.util.Optional;
import java.util.TreeSet;

public class L5W104MultiReleaseModulePath {
    public static void main(String[] args) {
        Optional<Module> m = ModuleLayer.boot().findModule("mrv");
        System.out.println("feature=" + Runtime.version().feature());
        System.out.println(m.isPresent()
                ? "mrv packages=" + new TreeSet<>(m.get().getPackages())
                : "mrv absent");
        Optional<Module> v = ModuleLayer.boot().findModule("vmi");
        System.out.println(v.isPresent()
                ? "vmi automatic=" + v.get().getDescriptor().isAutomatic()
                        + " exports=" + new TreeSet<>(v.get().getDescriptor().exports().stream()
                                .map(e -> e.source()).toList())
                : "vmi absent");
    }
}
