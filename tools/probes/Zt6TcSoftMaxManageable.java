// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.HotSpotDiagnosticMXBean;
import com.sun.management.VMOption;
import java.lang.management.ManagementFactory;

/**
 * zt t6/tc (zt1tp item 2): is {@code SoftMaxHeapSize} a manageable VM option
 * on ZGC, as on HotSpot 25 ({@code java -XX:+UseZGC -Xmx1g})? Prints only
 * deterministic lines: the option's value / writeable / origin before and
 * after a {@code setVMOption}, and the exception CLASS (not its message) of
 * an out-of-range write.
 *
 * <p>Run: {@code cratonvm -Xmx1g -cp tools/probes Zt6TcSoftMaxManageable} with
 * {@code CRATONVM_ZGC_SOFT_MAX_MANAGEABLE=1}; HotSpot twin
 * {@code java -XX:+UseZGC -Xmx1g -cp tools/probes Zt6TcSoftMaxManageable}.
 */
public class Zt6TcSoftMaxManageable {
    public static void main(String[] args) {
        HotSpotDiagnosticMXBean bean =
                ManagementFactory.getPlatformMXBean(HotSpotDiagnosticMXBean.class);
        VMOption before;
        try {
            before = bean.getVMOption("SoftMaxHeapSize");
        } catch (IllegalArgumentException e) {
            System.out.println("exists=false");
            return;
        }
        System.out.println("exists=true");
        System.out.println("before value=" + before.getValue()
                + " writeable=" + before.isWriteable()
                + " origin=" + before.getOrigin());
        try {
            bean.setVMOption("SoftMaxHeapSize", Long.toString(256L * 1024 * 1024));
            System.out.println("set=ok");
        } catch (RuntimeException e) {
            System.out.println("set=" + e.getClass().getName());
        }
        VMOption after = bean.getVMOption("SoftMaxHeapSize");
        System.out.println("after value=" + after.getValue()
                + " writeable=" + after.isWriteable()
                + " origin=" + after.getOrigin());
        try {
            bean.setVMOption("SoftMaxHeapSize", Long.toString(Long.MAX_VALUE / 2));
            System.out.println("too_big=ok");
        } catch (RuntimeException e) {
            System.out.println("too_big=" + e.getClass().getName());
        }
        System.out.println("final value=" + bean.getVMOption("SoftMaxHeapSize").getValue());
    }
}
