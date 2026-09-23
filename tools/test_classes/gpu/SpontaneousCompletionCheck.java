// Java-level counterpart to the hardware validation for known-issues
// followups item 3 (2026-07-12): a GpuFuture must complete on its own,
// with ZERO isDone()/getNow()/get() calls in between dispatch and
// completion — driven by the vm/src/runtime/offload.rs completion
// reaper thread, not by the application polling. (Since 2026-09-02 the
// reaper polls the completion event; the cuLaunchHostFunc callback it
// used to wake off is opt-in via CRATONVM_GPU_HOST_CALLBACK=1.)
//
// NOT YET RUN: this needs the external craton-gpu-java repo (the
// `craton.gpu.GpuExecutor`/`GpuFuture` classes below) checked out
// beside this workspace or at C:/craton/craton-gpu-java — see
// craton-gpu/build.rs. That repo isn't present on this box, so the
// actual 2026-07-12 hardware validation used the lower-level Rust
// entry point instead: `device_submission_completes_spontaneously_without_any_poll_call`
// in vm/tests/gpu_offload_features.rs, which dispatches through
// `dispatch_method_from_native` directly and reads `StreamSubmission::status`
// with no `GpuFuture`-equivalent call at all — passed on the RTX 2060.
// This file is kept as the intended Java-level equivalent for whenever
// the craton-gpu-java jar is available; the proof technique is the
// same (read the host array directly, no future API call), just one
// layer higher.
//
// The test below deliberately never calls any GpuFuture method between
// submit() and the sleep. It proves the reaper ran by reading the
// *host* output array directly during the sleep window: that array can
// only contain the kernel's result if `finalize_submission` (which
// drains the D2H writeback) already ran, and the only thing that can
// have run it without any GpuFuture call is the background reaper.
//
// Usage:
//   cratonvm --gpu --classpath test_classes/gpu:<craton-gpu-jar> \
//       SpontaneousCompletionCheck [n]

import craton.gpu.GpuExecutor;
import craton.gpu.GpuException;
import craton.gpu.GpuFuture;

public class SpontaneousCompletionCheck {
    public static void main(String[] args) throws InterruptedException {
        int n = (args.length > 0) ? Integer.parseInt(args[0]) : (1 << 22);
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
            b[i] = 2 * i;
        }

        try (GpuExecutor exec = GpuExecutor.open()) {
            // Warmup: get the kernel compiled + cached before we start
            // timing/observing anything.
            GpuFuture<Void> warm = exec.submit(
                "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);
            warm.get();

            // Reset the output array so "already written" is
            // distinguishable from "still the warmup's leftover value."
            java.util.Arrays.fill(out, 0);

            long t0 = System.nanoTime();
            GpuFuture<Void> f = exec.submit(
                "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);

            // NO GpuFuture method call of any kind here. Just sleep.
            Thread.sleep(500);

            boolean writtenBeforeAnyPollCall =
                (out[0] == a[0] + b[0]) && (out[n - 1] == a[n - 1] + b[n - 1]);
            long t1 = System.nanoTime();

            System.out.println(
                "slept_ms=" + (t1 - t0) / 1_000_000
                + " out_written_before_any_gpufuture_call=" + writtenBeforeAnyPollCall
                + " out[0]=" + out[0]
                + " out[n-1]=" + out[n - 1]
            );

            // Now finalize formally (releases resources) and confirm
            // correctness end to end.
            long t2 = System.nanoTime();
            f.get();
            long t3 = System.nanoTime();
            System.out.println(
                "get_after_sleep_ns=" + (t3 - t2)
                + " out[0]=" + out[0]
                + " out[n-1]=" + out[n - 1]
                + " expected0=" + (a[0] + b[0])
                + " expectedN=" + (a[n - 1] + b[n - 1])
            );

            boolean correct = (out[0] == a[0] + b[0]) && (out[n - 1] == a[n - 1] + b[n - 1]);
            if (!writtenBeforeAnyPollCall || !correct) {
                System.out.println("FAIL: reaper did not finalize spontaneously within the 500ms sleep");
                System.exit(1);
            }
            System.out.println("PASS: spontaneous completion confirmed on real hardware");
        } catch (GpuException e) {
            System.out.println("GpuException: " + e.getMessage());
            System.exit(2);
        } catch (NoClassDefFoundError e) {
            System.out.println(
                "NoClassDefFoundError: " + e.getMessage()
                + " (the craton-gpu jar is probably missing from --classpath)"
            );
            System.exit(3);
        }
    }
}
