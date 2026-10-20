package cratonvm;

/**
 * Target class used by NEW-19 module-access tests.
 *
 * Integration tests post-hoc re-assign this class to a synthetic named
 * module with NO exports/opens, so reflective access from `TckModule`
 * (kept in the unnamed module) must be denied by JEP 403 strong
 * encapsulation.
 */
public class ModuleTarget {
    private int secret = 42;
    private String name = "target";

    private int getSecret() {
        return secret;
    }

    public int publicValue() {
        return 7;
    }

    public ModuleTarget() {}

    public ModuleTarget(int s) {
        this.secret = s;
    }
}
