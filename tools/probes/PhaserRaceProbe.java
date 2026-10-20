import java.util.concurrent.Phaser;

/**
 * Falsifier for docs/known-issues/jdk-only/W6-12-stampedlock-split-brain.md's
 * Phaser fix: a call to getUnarrivedParties() between arrivals must not
 * corrupt the phaser's other counters.
 */
public class PhaserRaceProbe {
    public static void main(String[] args) throws Exception {
        Phaser ph = new Phaser(2);
        int regBefore = ph.getRegisteredParties();
        int arrBefore = ph.getArrivedParties();
        ph.arrive();
        int unarrived = ph.getUnarrivedParties();
        int regAfterProbe = ph.getRegisteredParties();
        int arrAfterProbe = ph.getArrivedParties();
        System.out.println("reg " + regBefore + "->" + regAfterProbe
                + " arr " + arrBefore + "->" + arrAfterProbe
                + " unarrived=" + unarrived);
        boolean corrupted = regAfterProbe != 2 || arrAfterProbe != 1;
        int phase = ph.arriveAndAwaitAdvance();
        System.out.println("phase=" + phase + " terminated=" + ph.isTerminated());
        boolean advanced = phase == 1 && !ph.isTerminated();
        System.out.println("PASS PhaserRaceProbe corrupted=" + corrupted + " advanced=" + advanced);
        if (corrupted || !advanced) {
            throw new AssertionError("Phaser state corrupted by getUnarrivedParties() between arrivals");
        }
    }
}
