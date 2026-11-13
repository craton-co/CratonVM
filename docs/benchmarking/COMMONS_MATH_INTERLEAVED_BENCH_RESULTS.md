# Apache Commons Math Full Suite Interleaved Benchmark: CVM vs HotSpot

**Date**: 2026-10-31 05:35:37 UTC  
**Host**: AMD EPYC-Turin (16 vCPUs, 60 GB RAM, Linux x86_64)  
**Configurations**: CratonVM Default (`/root/tc-bin/cratonvm`) vs OpenJDK HotSpot 25  
**Methodology**: Class-by-class interleaved execution with alternating order (1 iteration per class, 30s timeout per class)  

### Summary Metrics

- **Total Test Classes Evaluated**: 376
- **CratonVM PASS Count**: 315 / 376 (83.8%)
- **HotSpot 25 PASS Count**: 317 / 376 (84.3%)
- **Both Passed**: 314
- **Median Ratio (CVM / HotSpot)**: 3.67x
- **Mean Ratio (CVM / HotSpot)**: 3.89x
- **Classes Where CVM Was Faster**: 0 / 314

### Class-by-Class Results (First 50 Samples)

| # | Class Name | First Run | CVM Status | CVM (ms) | HotSpot Status | HotSpot (ms) | CVM / HS Ratio |
|---:|:---|:---:|:---:|---:|:---:|---:|---:|
| 1 | `org.apache.commons.math4.core.jdkmath.JdkMathTest` | CVM | PASS | 670 ms | PASS | 163 ms | 4.11x |
| 2 | `org.apache.commons.math4.legacy.TestUtils` | HotSpot | FAIL | 636 ms | FAIL | 179 ms | - |
| 3 | `org.apache.commons.math4.legacy.analysis.FunctionUtilsTest` | CVM | PASS | 1,223 ms | PASS | 350 ms | 3.49x |
| 4 | `org.apache.commons.math4.legacy.analysis.differentiation.DSCompilerTest` | HotSpot | PASS | 758 ms | PASS | 220 ms | 3.45x |
| 5 | `org.apache.commons.math4.legacy.analysis.differentiation.DerivativeStructureTest` | CVM | PASS | 2,911 ms | PASS | 807 ms | 3.61x |
| 6 | `org.apache.commons.math4.legacy.analysis.differentiation.FiniteDifferencesDifferentiatorTest` | HotSpot | PASS | 844 ms | PASS | 257 ms | 3.28x |
| 7 | `org.apache.commons.math4.legacy.analysis.differentiation.GradientFunctionTest` | CVM | PASS | 927 ms | PASS | 255 ms | 3.64x |
| 8 | `org.apache.commons.math4.legacy.analysis.differentiation.JacobianFunctionTest` | HotSpot | PASS | 688 ms | PASS | 191 ms | 3.6x |
| 9 | `org.apache.commons.math4.legacy.analysis.differentiation.SparseGradientTest` | CVM | PASS | 1,914 ms | PASS | 425 ms | 4.5x |
| 10 | `org.apache.commons.math4.legacy.analysis.function.BivariateFunctionTest` | HotSpot | PASS | 681 ms | PASS | 183 ms | 3.72x |
| 11 | `org.apache.commons.math4.legacy.analysis.function.GaussianTest` | CVM | PASS | 717 ms | PASS | 192 ms | 3.73x |
| 12 | `org.apache.commons.math4.legacy.analysis.function.HarmonicOscillatorTest` | HotSpot | PASS | 698 ms | PASS | 183 ms | 3.81x |
| 13 | `org.apache.commons.math4.legacy.analysis.function.LogisticTest` | CVM | PASS | 697 ms | PASS | 194 ms | 3.59x |
| 14 | `org.apache.commons.math4.legacy.analysis.function.LogitTest` | HotSpot | FAIL | 807 ms | FAIL | 232 ms | - |
| 15 | `org.apache.commons.math4.legacy.analysis.function.SigmoidTest` | CVM | PASS | 674 ms | PASS | 196 ms | 3.44x |
| 16 | `org.apache.commons.math4.legacy.analysis.function.SincTest` | HotSpot | PASS | 897 ms | PASS | 346 ms | 2.59x |
| 17 | `org.apache.commons.math4.legacy.analysis.function.SqrtTest` | CVM | PASS | 658 ms | PASS | 181 ms | 3.64x |
| 18 | `org.apache.commons.math4.legacy.analysis.function.StepFunctionTest` | HotSpot | PASS | 666 ms | PASS | 165 ms | 4.04x |
| 19 | `org.apache.commons.math4.legacy.analysis.function.UnivariateDifferentiableFunctionTest` | CVM | PASS | 705 ms | PASS | 186 ms | 3.79x |
| 20 | `org.apache.commons.math4.legacy.analysis.function.UnivariateFunctionTest` | HotSpot | PASS | 653 ms | PASS | 184 ms | 3.55x |
| 21 | `org.apache.commons.math4.legacy.analysis.integration.IterativeLegendreGaussIntegratorTest` | CVM | PASS | 8,782 ms | PASS | 958 ms | 9.17x |
| 22 | `org.apache.commons.math4.legacy.analysis.integration.MidPointIntegratorTest` | HotSpot | PASS | 691 ms | PASS | 205 ms | 3.37x |
| 23 | `org.apache.commons.math4.legacy.analysis.integration.RombergIntegratorTest` | CVM | PASS | 642 ms | PASS | 185 ms | 3.47x |
| 24 | `org.apache.commons.math4.legacy.analysis.integration.SimpsonIntegratorTest` | HotSpot | PASS | 668 ms | PASS | 198 ms | 3.37x |
| 25 | `org.apache.commons.math4.legacy.analysis.integration.TrapezoidIntegratorTest` | CVM | PASS | 656 ms | PASS | 186 ms | 3.53x |
| 26 | `org.apache.commons.math4.legacy.analysis.integration.gauss.BaseRuleFactoryTest` | HotSpot | PASS | 2,552 ms | PASS | 890 ms | 2.87x |
| 27 | `org.apache.commons.math4.legacy.analysis.integration.gauss.GaussIntegratorTest` | CVM | PASS | 662 ms | PASS | 166 ms | 3.99x |
| 28 | `org.apache.commons.math4.legacy.analysis.integration.gauss.GaussianQuadratureAbstractTest` | HotSpot | SKIP_ABSTRACT | 375 ms | SKIP_ABSTRACT | 80 ms | - |
| 29 | `org.apache.commons.math4.legacy.analysis.integration.gauss.HermiteParametricTest` | CVM | PASS | 1,301 ms | PASS | 237 ms | 5.49x |
| 30 | `org.apache.commons.math4.legacy.analysis.integration.gauss.HermiteTest` | HotSpot | PASS | 673 ms | PASS | 193 ms | 3.49x |
| 31 | `org.apache.commons.math4.legacy.analysis.integration.gauss.LaguerreTest` | CVM | PASS | 766 ms | PASS | 224 ms | 3.42x |
| 32 | `org.apache.commons.math4.legacy.analysis.integration.gauss.LegendreHighPrecisionParametricTest` | HotSpot | PASS | 4,366 ms | PASS | 856 ms | 5.1x |
| 33 | `org.apache.commons.math4.legacy.analysis.integration.gauss.LegendreHighPrecisionTest` | CVM | PASS | 27,236 ms | PASS | 2,952 ms | 9.23x |
| 34 | `org.apache.commons.math4.legacy.analysis.integration.gauss.LegendreParametricTest` | HotSpot | PASS | 971 ms | PASS | 253 ms | 3.84x |
| 35 | `org.apache.commons.math4.legacy.analysis.integration.gauss.LegendreTest` | CVM | PASS | 708 ms | PASS | 205 ms | 3.45x |
| 36 | `org.apache.commons.math4.legacy.analysis.interpolation.AkimaSplineInterpolatorTest` | HotSpot | PASS | 697 ms | PASS | 207 ms | 3.37x |
| 37 | `org.apache.commons.math4.legacy.analysis.interpolation.BicubicInterpolatingFunctionTest` | CVM | PASS | 777 ms | PASS | 241 ms | 3.22x |
| 38 | `org.apache.commons.math4.legacy.analysis.interpolation.BicubicInterpolatorTest` | HotSpot | PASS | 836 ms | PASS | 215 ms | 3.89x |
| 39 | `org.apache.commons.math4.legacy.analysis.interpolation.ClampedSplineInterpolatorTest` | CVM | PASS | 688 ms | PASS | 196 ms | 3.51x |
| 40 | `org.apache.commons.math4.legacy.analysis.interpolation.DividedDifferenceInterpolatorTest` | HotSpot | PASS | 659 ms | PASS | 190 ms | 3.47x |
| 41 | `org.apache.commons.math4.legacy.analysis.interpolation.FieldHermiteInterpolatorTest` | CVM | PASS | 3,005 ms | PASS | 872 ms | 3.45x |
| 42 | `org.apache.commons.math4.legacy.analysis.interpolation.HermiteInterpolatorTest` | HotSpot | PASS | 798 ms | PASS | 206 ms | 3.87x |
| 43 | `org.apache.commons.math4.legacy.analysis.interpolation.LinearInterpolatorTest` | CVM | PASS | 655 ms | PASS | 173 ms | 3.79x |
| 44 | `org.apache.commons.math4.legacy.analysis.interpolation.LoessInterpolatorTest` | HotSpot | PASS | 738 ms | PASS | 199 ms | 3.71x |
| 45 | `org.apache.commons.math4.legacy.analysis.interpolation.MicrosphereProjectionInterpolatorTest` | CVM | PASS | 824 ms | PASS | 232 ms | 3.55x |
| 46 | `org.apache.commons.math4.legacy.analysis.interpolation.NevilleInterpolatorTest` | HotSpot | PASS | 653 ms | PASS | 189 ms | 3.46x |
| 47 | `org.apache.commons.math4.legacy.analysis.interpolation.PiecewiseBicubicSplineInterpolatingFunctionTest` | CVM | PASS | 709 ms | PASS | 222 ms | 3.19x |
| 48 | `org.apache.commons.math4.legacy.analysis.interpolation.PiecewiseBicubicSplineInterpolatorTest` | HotSpot | PASS | 765 ms | PASS | 224 ms | 3.42x |
| 49 | `org.apache.commons.math4.legacy.analysis.interpolation.SplineInterpolatorTest` | CVM | PASS | 676 ms | PASS | 205 ms | 3.3x |
| 50 | `org.apache.commons.math4.legacy.analysis.interpolation.TricubicInterpolatingFunctionTest` | HotSpot | PASS | 1,037 ms | PASS | 337 ms | 3.08x |

*... Full table of all 376 classes available in `commons_math_interleaved_results.tsv` and `commons_math_interleaved_results.json`.* 

