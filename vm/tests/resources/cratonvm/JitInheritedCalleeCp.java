// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

/**
 * Fixture for the "inherited callee scanned against the SUBCLASS constant
 * pool" regression (see `vm/tests/jit_inherited_callee_constant_pool.rs`).
 *
 * `Sub` declares nothing but a default constructor, so its constant pool is
 * short. `Base.inheritedAccessor` is reached through a `Sub` receiver and its
 * body contains an `invokevirtual` — the only instruction kind the callee
 * compile gate's generic-metadata scan inspects. The `pad*` members below
 * exist to push that `invokevirtual`'s constant-pool index well past the end
 * of `Sub`'s pool, so resolving the index in the wrong pool yields "no such
 * entry" rather than accidentally landing on another method reference.
 */
public class JitInheritedCalleeCp {

	public static class Base {
		private final StringBuilder payload = new StringBuilder("payload");

		// Padding: distinct string constants and method references, emitted
		// into the constant pool ahead of `inheritedAccessor`'s own entries.
		private String pad00() { return "pad-constant-00"; }
		private String pad01() { return "pad-constant-01"; }
		private String pad02() { return "pad-constant-02"; }
		private String pad03() { return "pad-constant-03"; }
		private String pad04() { return "pad-constant-04"; }
		private String pad05() { return "pad-constant-05"; }
		private String pad06() { return "pad-constant-06"; }
		private String pad07() { return "pad-constant-07"; }
		private String pad08() { return "pad-constant-08"; }
		private String pad09() { return "pad-constant-09"; }
		private String pad10() { return "pad-constant-10"; }
		private String pad11() { return "pad-constant-11"; }
		private String pad12() { return "pad-constant-12"; }
		private String pad13() { return "pad-constant-13"; }
		private String pad14() { return "pad-constant-14"; }
		private String pad15() { return "pad-constant-15"; }

		public int padSum() {
			return pad00().length() + pad01().length() + pad02().length() + pad03().length()
					+ pad04().length() + pad05().length() + pad06().length() + pad07().length()
					+ pad08().length() + pad09().length() + pad10().length() + pad11().length()
					+ pad12().length() + pad13().length() + pad14().length() + pad15().length();
		}

		/** Inherited by `Sub`; body holds the `invokevirtual` the gate scans. */
		public int inheritedAccessor() {
			return payload.length();
		}
	}

	/** Declares nothing: short constant pool, inherits `inheritedAccessor`. */
	public static class Sub extends Base {
	}

	public static int drive(int iterations) {
		Base receiver = new Sub();
		int total = 0;
		for (int i = 0; i < iterations; i++) {
			total += receiver.inheritedAccessor();
		}
		return total;
	}
}
