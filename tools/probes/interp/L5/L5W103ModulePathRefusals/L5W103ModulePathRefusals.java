// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 103, lane L5 -- see setup.sh for the build, the
// run commands and the expected output of each module path.

import java.util.Optional;
import java.util.ServiceLoader;
import java.util.TreeSet;

public class L5W103ModulePathRefusals {
    public static void main(String[] args) {
        Optional<Module> mr = ModuleLayer.boot().findModule("mr");
        if (mr.isPresent()) {
            System.out.println("mr packages=" + new TreeSet<>(mr.get().getPackages())
                    + " automatic=" + mr.get().getDescriptor().isAutomatic());
        } else {
            System.out.println("ran");
        }
        ModuleLayer.boot().findModule("ver").ifPresent(m -> System.out.println(
                "ver rawVersion=" + m.getDescriptor().rawVersion().orElse("none")));
        ModuleLayer.boot().findModule("impl").ifPresent(m -> {
            System.out.println("impl provides=" + m.getDescriptor().provides()
                    + " mainClass=" + m.getDescriptor().mainClass().orElse("none"));
            System.out.println("runnable providers=" + ServiceLoader.load(Runnable.class).stream()
                    .map(p -> p.type().getName() + "@" + p.type().getModule().getName())
                    .sorted().toList());
        });
    }
}
