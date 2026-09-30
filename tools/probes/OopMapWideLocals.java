/**
 * Precise oop maps stop at 64 locals, and nothing says so.
 *
 * `compute_local_oop_masks` returns EMPTY vectors when `max_locals > 64`, and
 * `record_oop_map`'s Stage 2 is wrapped in `if !self.local_oop_masks.is_empty()`
 * -- so for such a method it contributes no slots, bumps no
 * `map_incomplete_cause`, and does NOT set `map_incomplete`. The map ships
 * naming no reference locals at all, while the frame-slot coverage claim
 * (`fully_oop_covered`) stays TRUE.
 *
 * The shadow half DOES fail closed (`shadow_incomplete_cause::TOO_MANY_LOCALS`),
 * so relocation is refused; but `fully_oop_covered` is consumed separately --
 * `conservative_roots`' coverage PIN reads `!cm.fully_oop_covered` -- and that
 * is the claim this probe is about.
 *
 * <p>80 reference locals, all live across allocations that can collect.
 *
 * <pre>
 *   javac -d $OUT probes/OopMapWideLocals.java
 *   CRATONVM_DBG=oopcov cratonvm --java-home $JDK25 --Xmx 256m -cp $OUT OopMapWideLocals
 * </pre>
 *
 * Read `frameslot=` and `shadow=` for `OopMapWideLocals.wide`, and
 * `scauses(... locals64=N ...)` beside `causes(...)`.
 */
public class OopMapWideLocals {
    static Object sink;
    static long guard;

    static long wide(Object seed) {
        long acc = 0;
        Object[] v0 = new Object[]{ seed, new int[8] };
        Object[] v1 = new Object[]{ seed, new int[9] };
        Object[] v2 = new Object[]{ seed, new int[10] };
        Object[] v3 = new Object[]{ seed, new int[11] };
        Object[] v4 = new Object[]{ seed, new int[12] };
        Object[] v5 = new Object[]{ seed, new int[13] };
        Object[] v6 = new Object[]{ seed, new int[14] };
        Object[] v7 = new Object[]{ seed, new int[15] };
        Object[] v8 = new Object[]{ seed, new int[16] };
        Object[] v9 = new Object[]{ seed, new int[17] };
        Object[] v10 = new Object[]{ seed, new int[18] };
        Object[] v11 = new Object[]{ seed, new int[19] };
        Object[] v12 = new Object[]{ seed, new int[20] };
        Object[] v13 = new Object[]{ seed, new int[8] };
        Object[] v14 = new Object[]{ seed, new int[9] };
        Object[] v15 = new Object[]{ seed, new int[10] };
        Object[] v16 = new Object[]{ seed, new int[11] };
        Object[] v17 = new Object[]{ seed, new int[12] };
        Object[] v18 = new Object[]{ seed, new int[13] };
        Object[] v19 = new Object[]{ seed, new int[14] };
        Object[] v20 = new Object[]{ seed, new int[15] };
        Object[] v21 = new Object[]{ seed, new int[16] };
        Object[] v22 = new Object[]{ seed, new int[17] };
        Object[] v23 = new Object[]{ seed, new int[18] };
        Object[] v24 = new Object[]{ seed, new int[19] };
        Object[] v25 = new Object[]{ seed, new int[20] };
        Object[] v26 = new Object[]{ seed, new int[8] };
        Object[] v27 = new Object[]{ seed, new int[9] };
        Object[] v28 = new Object[]{ seed, new int[10] };
        Object[] v29 = new Object[]{ seed, new int[11] };
        Object[] v30 = new Object[]{ seed, new int[12] };
        Object[] v31 = new Object[]{ seed, new int[13] };
        Object[] v32 = new Object[]{ seed, new int[14] };
        Object[] v33 = new Object[]{ seed, new int[15] };
        Object[] v34 = new Object[]{ seed, new int[16] };
        Object[] v35 = new Object[]{ seed, new int[17] };
        Object[] v36 = new Object[]{ seed, new int[18] };
        Object[] v37 = new Object[]{ seed, new int[19] };
        Object[] v38 = new Object[]{ seed, new int[20] };
        Object[] v39 = new Object[]{ seed, new int[8] };
        Object[] v40 = new Object[]{ seed, new int[9] };
        Object[] v41 = new Object[]{ seed, new int[10] };
        Object[] v42 = new Object[]{ seed, new int[11] };
        Object[] v43 = new Object[]{ seed, new int[12] };
        Object[] v44 = new Object[]{ seed, new int[13] };
        Object[] v45 = new Object[]{ seed, new int[14] };
        Object[] v46 = new Object[]{ seed, new int[15] };
        Object[] v47 = new Object[]{ seed, new int[16] };
        Object[] v48 = new Object[]{ seed, new int[17] };
        Object[] v49 = new Object[]{ seed, new int[18] };
        Object[] v50 = new Object[]{ seed, new int[19] };
        Object[] v51 = new Object[]{ seed, new int[20] };
        Object[] v52 = new Object[]{ seed, new int[8] };
        Object[] v53 = new Object[]{ seed, new int[9] };
        Object[] v54 = new Object[]{ seed, new int[10] };
        Object[] v55 = new Object[]{ seed, new int[11] };
        Object[] v56 = new Object[]{ seed, new int[12] };
        Object[] v57 = new Object[]{ seed, new int[13] };
        Object[] v58 = new Object[]{ seed, new int[14] };
        Object[] v59 = new Object[]{ seed, new int[15] };
        Object[] v60 = new Object[]{ seed, new int[16] };
        Object[] v61 = new Object[]{ seed, new int[17] };
        Object[] v62 = new Object[]{ seed, new int[18] };
        Object[] v63 = new Object[]{ seed, new int[19] };
        Object[] v64 = new Object[]{ seed, new int[20] };
        Object[] v65 = new Object[]{ seed, new int[8] };
        Object[] v66 = new Object[]{ seed, new int[9] };
        Object[] v67 = new Object[]{ seed, new int[10] };
        Object[] v68 = new Object[]{ seed, new int[11] };
        Object[] v69 = new Object[]{ seed, new int[12] };
        Object[] v70 = new Object[]{ seed, new int[13] };
        Object[] v71 = new Object[]{ seed, new int[14] };
        Object[] v72 = new Object[]{ seed, new int[15] };
        Object[] v73 = new Object[]{ seed, new int[16] };
        Object[] v74 = new Object[]{ seed, new int[17] };
        Object[] v75 = new Object[]{ seed, new int[18] };
        Object[] v76 = new Object[]{ seed, new int[19] };
        Object[] v77 = new Object[]{ seed, new int[20] };
        Object[] v78 = new Object[]{ seed, new int[8] };
        Object[] v79 = new Object[]{ seed, new int[9] };
        // Every vN above is still live here, across an allocation that can
        // collect. A precise map must name all 80 of them.
        sink = new Object[]{ seed, new int[256] };
        acc += ((int[]) v0[1]).length;
        acc += ((int[]) v1[1]).length;
        acc += ((int[]) v2[1]).length;
        acc += ((int[]) v3[1]).length;
        acc += ((int[]) v4[1]).length;
        acc += ((int[]) v5[1]).length;
        acc += ((int[]) v6[1]).length;
        acc += ((int[]) v7[1]).length;
        acc += ((int[]) v8[1]).length;
        acc += ((int[]) v9[1]).length;
        acc += ((int[]) v10[1]).length;
        acc += ((int[]) v11[1]).length;
        acc += ((int[]) v12[1]).length;
        acc += ((int[]) v13[1]).length;
        acc += ((int[]) v14[1]).length;
        acc += ((int[]) v15[1]).length;
        acc += ((int[]) v16[1]).length;
        acc += ((int[]) v17[1]).length;
        acc += ((int[]) v18[1]).length;
        acc += ((int[]) v19[1]).length;
        acc += ((int[]) v20[1]).length;
        acc += ((int[]) v21[1]).length;
        acc += ((int[]) v22[1]).length;
        acc += ((int[]) v23[1]).length;
        acc += ((int[]) v24[1]).length;
        acc += ((int[]) v25[1]).length;
        acc += ((int[]) v26[1]).length;
        acc += ((int[]) v27[1]).length;
        acc += ((int[]) v28[1]).length;
        acc += ((int[]) v29[1]).length;
        acc += ((int[]) v30[1]).length;
        acc += ((int[]) v31[1]).length;
        acc += ((int[]) v32[1]).length;
        acc += ((int[]) v33[1]).length;
        acc += ((int[]) v34[1]).length;
        acc += ((int[]) v35[1]).length;
        acc += ((int[]) v36[1]).length;
        acc += ((int[]) v37[1]).length;
        acc += ((int[]) v38[1]).length;
        acc += ((int[]) v39[1]).length;
        acc += ((int[]) v40[1]).length;
        acc += ((int[]) v41[1]).length;
        acc += ((int[]) v42[1]).length;
        acc += ((int[]) v43[1]).length;
        acc += ((int[]) v44[1]).length;
        acc += ((int[]) v45[1]).length;
        acc += ((int[]) v46[1]).length;
        acc += ((int[]) v47[1]).length;
        acc += ((int[]) v48[1]).length;
        acc += ((int[]) v49[1]).length;
        acc += ((int[]) v50[1]).length;
        acc += ((int[]) v51[1]).length;
        acc += ((int[]) v52[1]).length;
        acc += ((int[]) v53[1]).length;
        acc += ((int[]) v54[1]).length;
        acc += ((int[]) v55[1]).length;
        acc += ((int[]) v56[1]).length;
        acc += ((int[]) v57[1]).length;
        acc += ((int[]) v58[1]).length;
        acc += ((int[]) v59[1]).length;
        acc += ((int[]) v60[1]).length;
        acc += ((int[]) v61[1]).length;
        acc += ((int[]) v62[1]).length;
        acc += ((int[]) v63[1]).length;
        acc += ((int[]) v64[1]).length;
        acc += ((int[]) v65[1]).length;
        acc += ((int[]) v66[1]).length;
        acc += ((int[]) v67[1]).length;
        acc += ((int[]) v68[1]).length;
        acc += ((int[]) v69[1]).length;
        acc += ((int[]) v70[1]).length;
        acc += ((int[]) v71[1]).length;
        acc += ((int[]) v72[1]).length;
        acc += ((int[]) v73[1]).length;
        acc += ((int[]) v74[1]).length;
        acc += ((int[]) v75[1]).length;
        acc += ((int[]) v76[1]).length;
        acc += ((int[]) v77[1]).length;
        acc += ((int[]) v78[1]).length;
        acc += ((int[]) v79[1]).length;
        sink = new Object[]{ seed, new int[512] };
        acc += v0.length;
        acc += v1.length;
        acc += v2.length;
        acc += v3.length;
        acc += v4.length;
        acc += v5.length;
        acc += v6.length;
        acc += v7.length;
        acc += v8.length;
        acc += v9.length;
        acc += v10.length;
        acc += v11.length;
        acc += v12.length;
        acc += v13.length;
        acc += v14.length;
        acc += v15.length;
        acc += v16.length;
        acc += v17.length;
        acc += v18.length;
        acc += v19.length;
        acc += v20.length;
        acc += v21.length;
        acc += v22.length;
        acc += v23.length;
        acc += v24.length;
        acc += v25.length;
        acc += v26.length;
        acc += v27.length;
        acc += v28.length;
        acc += v29.length;
        acc += v30.length;
        acc += v31.length;
        acc += v32.length;
        acc += v33.length;
        acc += v34.length;
        acc += v35.length;
        acc += v36.length;
        acc += v37.length;
        acc += v38.length;
        acc += v39.length;
        acc += v40.length;
        acc += v41.length;
        acc += v42.length;
        acc += v43.length;
        acc += v44.length;
        acc += v45.length;
        acc += v46.length;
        acc += v47.length;
        acc += v48.length;
        acc += v49.length;
        acc += v50.length;
        acc += v51.length;
        acc += v52.length;
        acc += v53.length;
        acc += v54.length;
        acc += v55.length;
        acc += v56.length;
        acc += v57.length;
        acc += v58.length;
        acc += v59.length;
        acc += v60.length;
        acc += v61.length;
        acc += v62.length;
        acc += v63.length;
        acc += v64.length;
        acc += v65.length;
        acc += v66.length;
        acc += v67.length;
        acc += v68.length;
        acc += v69.length;
        acc += v70.length;
        acc += v71.length;
        acc += v72.length;
        acc += v73.length;
        acc += v74.length;
        acc += v75.length;
        acc += v76.length;
        acc += v77.length;
        acc += v78.length;
        acc += v79.length;
        return acc;
    }

    public static void main(String[] args) {
        long t = 0;
        Object seed = new int[4];
        for (int r = 0; r < 30000; r++) {
            t += wide(seed);
        }
        guard = t;
        System.out.println("PASS OopMapWideLocals total=" + t);
    }
}
