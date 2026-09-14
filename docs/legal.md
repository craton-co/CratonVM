# JCK Licensing and Legal Requirements

## Overview

The Java Compatibility Kit (JCK) is Oracle's official test suite for verifying
that a Java SE implementation conforms to the specification. Running the JCK
requires a license agreement with Oracle. This document describes the licensing
options, application process, and the current status for CratonVM.

## License Options

### OCTLA (Oracle Technology Compatibility Kit License Agreement)

The OCTLA grants **non-commercial** access to the TCK (Technology Compatibility
Kit) for implementations of Java SE. Key points:

- Available to individuals, academic institutions, and non-commercial projects.
- Grants the right to download and execute the JCK test suite against a
  Java SE implementation under development.
- The implementation under test must target a specific Java SE version (CratonVM
  targets **OpenJDK 25 / Java SE 25**).
- Results may not be used for marketing or compatibility claims unless the
  implementation passes the full TCK and the licensee signs an additional
  trademark license.
- The OCTLA does not permit redistribution of the JCK binaries or test sources.

### TCK Community License (for open-source implementations)

Oracle provides a separate license path for qualifying open-source Java SE
implementations:

- The implementation must be released under an OSI-approved open-source license.
- The license covers TCK access specifically for achieving compatibility of an
  open-source runtime.
- The applicant must demonstrate a credible implementation effort (partial VM
  with meaningful class library support).
- Passing the full TCK under this license allows the implementation to use the
  "Java Compatible" designation.

## Application Process

1. **Identify the target specification.** CratonVM targets Java SE 25 (OpenJDK 25).
2. **Choose the license path.** For CratonVM, either the OCTLA (non-commercial)
   or the TCK Community License (open-source) is applicable.
3. **Submit an application** through Oracle's TCK program page at
   `https://openjdk.org/groups/conformance/JckAccess/`. The application requires:
   - Project name and URL.
   - Description of the implementation (language, architecture, current status).
   - Target Java SE version.
   - Contact information for the project lead.
4. **Review period.** Oracle reviews the application and may request additional
   information about the implementation's maturity.
5. **License execution.** Upon approval, the applicant signs the license
   agreement and receives access credentials for the JCK download portal.
6. **Download the JCK.** The JCK bundle includes the JavaTest harness, test
   classes, and configuration templates.

## Requirements for CratonVM

- **Target version:** Java SE 25 (OpenJDK 25).
- **Implementation language:** Rust (with a bytecode interpreter, x86-64 JIT,
  and native method bridge).
- **Class library strategy:** Native Rust implementations of `java.base` module
  classes, registered through the VM's native method registry.
- **Test infrastructure:** The `.github/workflows/jck.yml`
  CI workflow (manual `workflow_dispatch` only — see RELEASING.md §2) and
  `bench/javatest_config.jti` harness configuration are prepared and ready to
  execute once JCK access is granted.
- **Failure tracking:** The `vm/tests/jck_harness.rs` test captures per-test
  failures into `bench/jck-failures.json` for regression tracking. Failure
  categories (ClassNotFound, NativeMethodNotFound, RuntimeError, etc.) are
  pre-defined in the JSON schema.
- **Differential testing:** The `vm/tests/differential.rs` harness runs
  methods under both CratonVM and HotSpot to detect behavioral divergences.
  Divergences are written to `bench/differential-divergences.json`; the
  regression fixtures live in `vm/tests/resources/cratonvm/Diff*.java`, and
  `DIFFERENTIAL_CLASSES` runs an ad-hoc sweep over any additional classes. No
  divergence is currently open (the standing log was retired once
  its last entry was fixed).

## Current Status

| Item                  | Status              |
|-----------------------|---------------------|
| License path          | OCTLA or TCK Community License |
| Application           | **Pending application** |
| JCK version target    | JCK 25 (Java SE 25) |
| CI runner prepared    | Yes (`.github/workflows/jck.yml`, manual `workflow_dispatch` only — see RELEASING.md §2) |
| Harness configuration | Yes (`bench/javatest_config.jti`) |
| Test harness          | Yes (`vm/tests/jck_harness.rs`) |
| Failure capture       | Yes (`bench/jck-failures.json`) |
| Differential testing  | Yes (`vm/tests/differential.rs`) |
| Divergence report     | Yes (`bench/differential-divergences.json`) |

The JCK license application has not yet been submitted. All CI and harness
infrastructure is in place and will activate automatically once `JCK_HOME` is
set in the CI environment after license approval and JCK download.
