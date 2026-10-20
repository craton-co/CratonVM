// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 103, lane L5 -- the provider and main class of
// the automatic module `impl` (see ../../../setup.sh).
package impl;

public class Hello implements Runnable {
    public void run() {
    }

    public static void main(String[] args) {
        System.out.println("hello from " + Hello.class.getModule().getName());
    }
}
