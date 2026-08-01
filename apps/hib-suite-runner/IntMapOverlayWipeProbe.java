import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * Targeted witness for a silent wipe of a live {@code HashMap<Integer,?>}.
 *
 * <p>CratonVM stores a fresh exact-class {@code HashMap} with boxed-Integer keys
 * in a Rust-side overlay ({@code hm_int_fast}) rather than in heap nodes, keyed
 * by a per-object handle. A thread-local single-entry memo caches
 * {@code (raw pointer, identity hash) -> handle} for the last such map the
 * thread touched, and {@code HashMap.<init>} evicts the overlay entry the memo
 * names whenever the new map's raw address equals the memo's — on the
 * assumption that the address's previous tenant must be dead.
 *
 * <p>That assumption fails when the previous tenant did not die but MOVED: a
 * copying young collection (or a promotion) relocates it and frees its old
 * address, a later allocation reuses that address, and the still-live map's
 * overlay entry — its only storage — is dropped. It then reads as empty
 * forever, with no exception anywhere.
 *
 * <p>This probe makes that sequence likely instead of astronomically rare:
 * hold a set of Integer-keyed maps alive, arrange for one of them to be the
 * last one the thread touched (so the memo names it), churn the young
 * generation hard enough to relocate it, then allocate a large run of bare
 * {@code HashMap}s so one of them lands on the freed address. Every victim is
 * then re-read.
 *
 * <p>usage: {@code IntMapOverlayWipeProbe [rounds] [victims] [churn]} — prints
 * {@code @@OVERLAYWIPE rounds=N wipes=M} and exits non-zero on any wipe.
 */
public final class IntMapOverlayWipeProbe {

	private static final int ENTRIES_PER_VICTIM = 3;

	public static void main(String[] args) {
		final int rounds = args.length > 0 ? Integer.parseInt( args[0] ) : 400;
		final int victimCount = args.length > 1 ? Integer.parseInt( args[1] ) : 64;
		final int churn = args.length > 2 ? Integer.parseInt( args[2] ) : 20000;

		final List<String> wipes = new ArrayList<>();
		// Survives every round so the victims are old enough to be promoted
		// rather than simply collected.
		final List<Map<Integer, String>> longLived = new ArrayList<>();

		for ( int round = 0; round < rounds && wipes.size() < 20; round++ ) {
			final List<Map<Integer, String>> victims = new ArrayList<>( victimCount );
			for ( int v = 0; v < victimCount; v++ ) {
				final Map<Integer, String> victim = new HashMap<>();
				for ( int k = 1; k <= ENTRIES_PER_VICTIM; k++ ) {
					victim.put( k, "r" + round + "v" + v + "k" + k );
				}
				victims.add( victim );
			}
			// Keep a slice alive across rounds: a map that has been promoted to
			// the old generation still has its young-generation address freed,
			// which is the recycling this probe needs.
			longLived.add( victims.get( 0 ) );
			if ( longLived.size() > 256 ) {
				longLived.remove( 0 );
			}

			// The memo is a single entry, so only the LAST Integer-keyed map the
			// thread touches is exposed. Touch one deliberately and then avoid
			// every other Integer-keyed map operation until the check.
			final Map<Integer, String> memoOwner = victims.get( victims.size() - 1 );
			memoOwner.get( 1 );

			// Churn: young-generation pressure to force collections (which
			// relocate or promote the victims and free their old addresses),
			// interleaved with bare HashMap allocations to land one on a freed
			// address. `new HashMap<>()` runs the constructor that performs the
			// eviction but does not itself update the memo.
			Object sink = null;
			for ( int i = 0; i < churn; i++ ) {
				sink = new HashMap<String, String>();
				if ( (i & 7) == 0 ) {
					sink = new byte[256];
				}
			}
			if ( sink == null ) {
				wipes.add( "round=" + round + " impossible: churn sink null" );
			}

			for ( int v = 0; v < victims.size(); v++ ) {
				final Map<Integer, String> victim = victims.get( v );
				final String failure = verify( victim, round, v );
				if ( failure != null ) {
					wipes.add( failure );
					if ( wipes.size() >= 20 ) {
						break;
					}
				}
			}
			for ( int v = 0; v < longLived.size() && wipes.size() < 20; v++ ) {
				final String failure = verify( longLived.get( v ), round, -1 - v );
				if ( failure != null ) {
					wipes.add( failure );
				}
			}
		}

		for ( String line : wipes ) {
			System.out.println( "OVERLAYWIPE " + line );
		}
		System.out.println( "@@OVERLAYWIPE rounds=" + rounds + " victims=" + victimCount
				+ " churn=" + churn + " wipes=" + wipes.size() );
		if ( !wipes.isEmpty() ) {
			System.exit( 1 );
		}
	}

	/** Every entry put into a victim must still be readable, by key and by size. */
	private static String verify(Map<Integer, String> victim, int round, int index) {
		if ( victim.size() != ENTRIES_PER_VICTIM ) {
			return "round=" + round + " victim=" + index + " size=" + victim.size()
					+ " expected=" + ENTRIES_PER_VICTIM;
		}
		for ( int k = 1; k <= ENTRIES_PER_VICTIM; k++ ) {
			if ( victim.get( k ) == null ) {
				return "round=" + round + " victim=" + index + " get(" + k
						+ ") null with size=" + victim.size();
			}
		}
		return null;
	}
}
