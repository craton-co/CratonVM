// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane SMALLFIX: the scalar-intrinsic switch is read per call
//! (IR2-5), and `StrictMath.abs(F/D)` lowers like `Math.abs(F/D)`.

use super::*;
use cratonvm_types::flags::with_thread_overrides;





