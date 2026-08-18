// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.TestExecutionListener;
import org.junit.platform.launcher.TestIdentifier;
import org.junit.platform.launcher.core.LauncherFactory;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.engine.TestExecutionResult;
import static org.junit.platform.engine.discovery.DiscoverySelectors.selectClass;

/**
 * SbRunner + a listener that announces every test as it starts and finishes.
 *
 * `@WebEndpointTest` is a parameterized TEMPLATE, so `selectMethod(fqcn, name)`
 * cannot address one test — the only way to find which of the 45 spins is to
 * run the class and watch which one starts and never finishes.
 * Unbuffered and flushed per line so a kill still leaves the last START behind.
 */
public class SbRunnerTrace {
    public static void main(String[] args) throws Exception {
        String fqcn = args[0];
        LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
                .selectors(selectClass(fqcn)).build();
        Launcher launcher = LauncherFactory.create();
        launcher.registerTestExecutionListeners(new TestExecutionListener() {
            @Override public void executionStarted(TestIdentifier id) {
                if (id.isTest()) say("@@START " + id.getDisplayName() + " | " + id.getUniqueId());
            }
            @Override public void executionFinished(TestIdentifier id, TestExecutionResult r) {
                if (id.isTest()) say("@@DONE  " + r.getStatus() + " " + id.getDisplayName());
            }
            private void say(String s) { System.out.println(s); System.out.flush(); }
        });
        launcher.execute(req);
        System.out.println("@@ALLDONE");
        System.out.flush();
        System.exit(0);
    }
}
