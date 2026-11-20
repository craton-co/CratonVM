package com.cratonvm.jdkonly.svc.open;

/**
 * Lives in an EXPORTED and OPENED package: deep reflection on its private state
 * must succeed from the unnamed module.
 */
public final class Opened {
    private String secret = "opened-private";

    public Opened() {
    }

    public String visible() {
        return "opened-public";
    }
}
