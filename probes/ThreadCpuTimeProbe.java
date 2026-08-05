import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;

/**
 * Does this VM report per-thread CPU time? On a shared box at 100% load,
 * wall-clock A/B cannot resolve a 2x effect, and per-thread CPU time is the
 * only in-process substitute. Prints the two numbers side by side after a
 * fixed spin: if `cpu_ms` tracks `wall_ms`, the clock is usable.
 */
public final class ThreadCpuTimeProbe {
	private ThreadCpuTimeProbe() {}

	static long sink;

	public static void main(String[] args) {
		ThreadMXBean bean;
		try {
			bean = ManagementFactory.getThreadMXBean();
		} catch (Throwable t) {
			System.out.println("@@CPUTIME unavailable getThreadMXBean: " + t);
			return;
		}
		boolean supported;
		try {
			supported = bean.isCurrentThreadCpuTimeSupported();
		} catch (Throwable t) {
			System.out.println("@@CPUTIME unavailable isCurrentThreadCpuTimeSupported: " + t);
			return;
		}
		long c0;
		try {
			c0 = bean.getCurrentThreadCpuTime();
		} catch (Throwable t) {
			System.out.println("@@CPUTIME unavailable getCurrentThreadCpuTime: " + t);
			return;
		}
		long w0 = System.nanoTime();
		for (int i = 0; i < 20_000_000; i++) {
			sink += i ^ (sink >>> 3);
		}
		long wall = System.nanoTime() - w0;
		long cpu = bean.getCurrentThreadCpuTime() - c0;
		System.out.printf("@@CPUTIME supported=%b wall_ms=%d cpu_ms=%d sink=%d%n",
				supported, wall / 1_000_000L, cpu / 1_000_000L, sink);
	}
}
