import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.List;

import org.hibernate.Session;
import org.hibernate.SessionFactory;
import org.hibernate.Transaction;
import org.hibernate.cfg.Configuration;

import jakarta.persistence.Entity;
import jakarta.persistence.GeneratedValue;
import jakarta.persistence.Id;
import jakarta.persistence.Table;
import jakarta.persistence.criteria.CriteriaBuilder;
import jakarta.persistence.criteria.CriteriaQuery;
import jakarta.persistence.criteria.Predicate;
import jakarta.persistence.criteria.Root;

/**
 * Phase-resolved stand-in for
 * {@code org.hibernate.orm.test.jpa.criteria.InPredicateTest#testInPredicate},
 * used to attribute the JIT-on/`--nojit` wall-clock split documented in
 * {@code docs/known-issues/hibernate/hib-inpredicate-dispatch-heavy-jit-timeout-*.md}
 * to a phase instead of to the whole class.
 *
 * It drives the REAL Hibernate stack (same jars, same `hibernate.properties`,
 * same criteria API calls) — only the JUnit/`SessionFactoryScope` wrapper is
 * replaced — because a hand-written replica of the criteria code would not
 * exercise the metamodel dispatch this note is about.
 *
 * Usage: {@code InPredPhaseProbe [elements] [reps]} (defaults 100000 1).
 * Prints one `@@PHASE` line per rep with per-phase milliseconds.
 */
public final class InPredPhaseProbe {
	private InPredPhaseProbe() {}

	@Entity(name = "ProbeEvent")
	@Table(name = "PROBE_EVENT_TABLE")
	public static class Event {
		@Id
		@GeneratedValue
		private Long id;

		private String name;

		public Event() {
		}

		public Long getId() {
			return id;
		}

		public String getName() {
			return name;
		}
	}

	private static final ThreadMXBean THREADS = ManagementFactory.getThreadMXBean();

	/** Current thread's CPU time, or 0 when the VM does not report it. */
	private static long cpuNanos() {
		try {
			return THREADS.getCurrentThreadCpuTime();
		} catch (Throwable t) {
			return 0L;
		}
	}

	public static void main(String[] args) {
		// args[0] is a comma-separated list of IN-list sizes, so one JVM can
		// walk a size ladder: a quadratic phase is only visible as a shape
		// across sizes, never from a single 100k data point.
		String[] sizeSpec = (args.length > 0 ? args[0] : "100000").split(",");
		int[] sizes = new int[sizeSpec.length];
		for (int i = 0; i < sizeSpec.length; i++) {
			sizes[i] = Integer.parseInt(sizeSpec[i].trim());
		}
		int reps = args.length > 1 ? Integer.parseInt(args[1]) : 1;

		long t0 = System.nanoTime();
		Configuration cfg = new Configuration();
		cfg.addAnnotatedClass(Event.class);
		SessionFactory sf = cfg.buildSessionFactory();
		long t1 = System.nanoTime();
		System.out.printf("@@PHASE bootstrap ms=%d%n", (t1 - t0) / 1_000_000L);
		System.out.flush();

		for (int rep = 0; rep < reps; rep++) {
		for (int elements : sizes) {
			try (Session session = sf.openSession()) {
				Transaction tx = session.beginTransaction();
				long c0 = cpuNanos();
				long p0 = System.nanoTime();
				CriteriaBuilder cb = session.getCriteriaBuilder();
				CriteriaQuery<Event> cr = cb.createQuery(Event.class);
				Root<Event> root = cr.from(Event.class);
				long p1 = System.nanoTime();
				List<String> names = new ArrayList<>(elements);
				for (int i = 0; i < elements; i++) {
					names.add("abc" + i);
				}
				long p2 = System.nanoTime();
				long c2 = cpuNanos();
				Predicate pred = root.get("name").in(names);
				long p3 = System.nanoTime();
				long c3 = cpuNanos();
				cr.select(root).where(pred);
				long p4 = System.nanoTime();
				long c4 = cpuNanos();
				session.createQuery(cr);
				long p5 = System.nanoTime();
				long c5 = cpuNanos();
				tx.rollback();
				System.out.printf(
						"@@PHASE rep=%d n=%d criteria=%d names=%d in=%d where=%d createQuery=%d total=%d%n",
						rep,
						elements,
						(p1 - p0) / 1_000_000L,
						(p2 - p1) / 1_000_000L,
						(p3 - p2) / 1_000_000L,
						(p4 - p3) / 1_000_000L,
						(p5 - p4) / 1_000_000L,
						(p5 - p0) / 1_000_000L);
				// CPU time as well as wall: this box is shared and routinely
				// runs at 100%, where wall-clock cannot resolve a 2x effect
				// (one measured arm moved 3x between identical runs).
				System.out.printf("@@CPU rep=%d n=%d in=%d createQuery=%d total=%d%n",
						rep,
						elements,
						(c3 - c2) / 1_000_000L,
						(c5 - c4) / 1_000_000L,
						(c5 - c0) / 1_000_000L);
				System.out.flush();
			}
		}
		}
		sf.close();
		System.out.println("@@PROBEEND ok");
	}
}
