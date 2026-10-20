// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 100, lane L5 -- see setup.sh for the build, the
// run commands and the expected output per booted JDK.

import java.io.InputStream;

public class L5W100MultiReleaseFeature {
    public static void main(String[] args) throws Exception {
        String res;
        try (InputStream in = L5W100MultiReleaseFeature.class.getClassLoader()
                .getResourceAsStream("mr/v.txt")) {
            res = in == null ? "null" : new String(in.readAllBytes(), "UTF-8");
        }
        System.out.println("feature=" + Runtime.version().feature()
                + " class=" + mr.V.v() + " resource=" + res);
    }
}
