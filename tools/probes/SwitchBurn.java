// Prices ONE interpreted `tableswitch` / `lookupswitch`, both of which fall
// through to the decoded handler because the dispatch loop has no fast arm for
// them. Each arm's body is the same accumulate; only the selector differs.
//   table  -> dense cases, javac emits tableswitch
//   lookup -> sparse cases, javac emits lookupswitch
//   ifchain-> the same selection written as an if-chain (all fast-path arms)
//   ctl    -> the loop with no selection at all
public class SwitchBurn {
    public static void main(String[] a) {
        String mode = a[0]; int n = Integer.parseInt(a[1]); int s = 0;
        if (mode.equals("table")) {
            for (int i = 0; i < n; i++) {
                switch (i & 7) {
                    case 0: s += 1; break; case 1: s += 2; break;
                    case 2: s += 3; break; case 3: s += 4; break;
                    case 4: s += 5; break; case 5: s += 6; break;
                    case 6: s += 7; break; default: s += 8; break;
                }
            }
        } else if (mode.equals("lookup")) {
            for (int i = 0; i < n; i++) {
                switch (i & 7) {
                    case 0: s += 1; break; case 11: s += 2; break;
                    case 202: s += 3; break; case 3003: s += 4; break;
                    case 40004: s += 5; break; case 500005: s += 6; break;
                    case 6000006: s += 7; break; default: s += 8; break;
                }
            }
        } else if (mode.equals("ifchain")) {
            for (int i = 0; i < n; i++) {
                int k = i & 7;
                if (k == 0) s += 1; else if (k == 1) s += 2;
                else if (k == 2) s += 3; else if (k == 3) s += 4;
                else if (k == 4) s += 5; else if (k == 5) s += 6;
                else if (k == 6) s += 7; else s += 8;
            }
        } else {
            for (int i = 0; i < n; i++) { s += (i & 7) + 1; }
        }
        if (s == 42) System.out.println("x");
    }
}
