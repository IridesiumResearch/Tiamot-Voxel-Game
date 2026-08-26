// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Sine and cosine that are the same number on every machine.
//!
//! # Why this file exists at all
//!
//! Charter rule 4 bans `sin` and `cos` from simulation outright: they are libm
//! calls, and libm differs between operating systems, libc versions and CPUs.
//! Nothing in the tick may call them, and until now nothing needed to — the
//! client rotates a movement vector before sending it, which is why
//! `PlayerInput::movement` is in world space.
//!
//! Then a mod wanted to throw something in the direction a player is facing.
//! That is a trig value the simulation genuinely needs, and the charter names
//! the answer: **"use a committed lookup table with linear interpolation —
//! deterministic by construction"**. This is that table.
//!
//! # What makes it deterministic
//!
//! The values are COMMITTED, as `f32` bit patterns, so no machine ever computes
//! them. Everything done to them here is `+`, `-`, `*` and comparison, which
//! Rust guarantees to be bit-identical to IEEE 754 on every supported target.
//! A table computed at startup would be a table two platforms could disagree
//! about, which is the whole problem restated.
//!
//! # Accuracy
//!
//! [`QUARTER`] samples over a quarter turn, linearly interpolated. The error of
//! linear interpolation is bounded by `|f''| h² / 8`, and with `|sin''| <= 1`
//! and `h = (pi/2)/QUARTER` that is under five parts in a million — far below
//! what a facing direction, a spawn offset or a thrown velocity can notice, and
//! it is the SAME error everywhere, which is the point.

/// Samples per quarter turn. One more than this is stored, so the last entry is
/// exactly `1.0` and `sin(pi/2)` needs no interpolation.
pub const QUARTER: usize = 256;

/// Two pi, as the table's own turn.
const TAU: f32 = std::f32::consts::TAU;

/// `sin` over `0..=pi/2`, as `f32` bit patterns.
///
/// **Bit patterns rather than decimal literals**: a decimal is a request for a
/// float and this is the float. Generated once and committed; nothing rebuilds
/// it, which is what makes every machine agree.
static SIN_QUARTER: [u32; QUARTER + 1] = [
    0x0000_0000,
    0x3bc9_0f88,
    0x3c49_0e90,
    0x3c96_c9b6,
    0x3cc9_0ab0,
    0x3cfb_49ba,
    0x3d16_c32c,
    0x3d2f_e007,
    0x3d48_fb30,
    0x3d62_1469,
    0x3d7b_2b74,
    0x3d8a_200a,
    0x3d96_a905,
    0x3da3_308c,
    0x3daf_b680,
    0x3dbc_3ac3,
    0x3dc8_bd36,
    0x3dd5_3db9,
    0x3de1_bc2e,
    0x3dee_3876,
    0x3dfa_b273,
    0x3e03_9502,
    0x3e09_cf86,
    0x3e10_08b7,
    0x3e16_4083,
    0x3e1c_76de,
    0x3e22_abb6,
    0x3e28_defc,
    0x3e2f_10a2,
    0x3e35_4098,
    0x3e3b_6ecf,
    0x3e41_9b37,
    0x3e47_c5c2,
    0x3e4d_ee60,
    0x3e54_1501,
    0x3e5a_3997,
    0x3e60_5c13,
    0x3e66_7c66,
    0x3e6c_9a7f,
    0x3e72_b651,
    0x3e78_cfcc,
    0x3e7e_e6e1,
    0x3e82_7dc0,
    0x3e85_86ce,
    0x3e88_8e93,
    0x3e8b_9507,
    0x3e8e_9a22,
    0x3e91_9ddd,
    0x3e94_a031,
    0x3e97_a117,
    0x3e9a_a086,
    0x3e9d_9e78,
    0x3ea0_9ae5,
    0x3ea3_95c5,
    0x3ea6_8f12,
    0x3ea9_86c4,
    0x3eac_7cd4,
    0x3eaf_713a,
    0x3eb2_63ef,
    0x3eb5_54ec,
    0x3eb8_442a,
    0x3ebb_31a0,
    0x3ebe_1d4a,
    0x3ec1_071e,
    0x3ec3_ef15,
    0x3ec6_d529,
    0x3ec9_b953,
    0x3ecc_9b8b,
    0x3ecf_7bca,
    0x3ed2_5a09,
    0x3ed5_3641,
    0x3ed8_106b,
    0x3eda_e880,
    0x3edd_be79,
    0x3ee0_924f,
    0x3ee3_63fa,
    0x3ee6_3375,
    0x3ee9_00b7,
    0x3eeb_cbbb,
    0x3eee_9479,
    0x3ef1_5aea,
    0x3ef4_1f07,
    0x3ef6_e0cb,
    0x3ef9_a02d,
    0x3efc_5d27,
    0x3eff_17b2,
    0x3f00_e7e4,
    0x3f02_42b1,
    0x3f03_9c3d,
    0x3f04_f484,
    0x3f06_4b82,
    0x3f07_a136,
    0x3f08_f59b,
    0x3f0a_48ad,
    0x3f0b_9a6b,
    0x3f0c_ead0,
    0x3f0e_39da,
    0x3f0f_8784,
    0x3f10_d3cd,
    0x3f12_1eb0,
    0x3f13_682a,
    0x3f14_b039,
    0x3f15_f6d9,
    0x3f17_3c07,
    0x3f18_7fc0,
    0x3f19_c200,
    0x3f1b_02c6,
    0x3f1c_420c,
    0x3f1d_7fd1,
    0x3f1e_bc12,
    0x3f1f_f6cb,
    0x3f21_2ff9,
    0x3f22_6799,
    0x3f23_9da9,
    0x3f24_d225,
    0x3f26_050a,
    0x3f27_3656,
    0x3f28_6605,
    0x3f29_9415,
    0x3f2a_c082,
    0x3f2b_eb4a,
    0x3f2d_1469,
    0x3f2e_3bde,
    0x3f2f_61a5,
    0x3f30_85bb,
    0x3f31_a81d,
    0x3f32_c8c9,
    0x3f33_e7bc,
    0x3f35_04f3,
    0x3f36_206c,
    0x3f37_3a23,
    0x3f38_5216,
    0x3f39_6842,
    0x3f3a_7ca4,
    0x3f3b_8f3b,
    0x3f3c_a003,
    0x3f3d_aef9,
    0x3f3e_bc1b,
    0x3f3f_c767,
    0x3f40_d0da,
    0x3f41_d870,
    0x3f42_de29,
    0x3f43_e200,
    0x3f44_e3f5,
    0x3f45_e403,
    0x3f46_e22a,
    0x3f47_de65,
    0x3f48_d8b3,
    0x3f49_d112,
    0x3f4a_c77f,
    0x3f4b_bbf8,
    0x3f4c_ae79,
    0x3f4d_9f02,
    0x3f4e_8d90,
    0x3f4f_7a1f,
    0x3f50_64af,
    0x3f51_4d3d,
    0x3f52_33c6,
    0x3f53_1849,
    0x3f53_fac3,
    0x3f54_db31,
    0x3f55_b993,
    0x3f56_95e5,
    0x3f57_7026,
    0x3f58_4853,
    0x3f59_1e6a,
    0x3f59_f26a,
    0x3f5a_c450,
    0x3f5b_941a,
    0x3f5c_61c7,
    0x3f5d_2d53,
    0x3f5d_f6be,
    0x3f5e_be05,
    0x3f5f_8327,
    0x3f60_4621,
    0x3f61_06f2,
    0x3f61_c598,
    0x3f62_8210,
    0x3f63_3c5a,
    0x3f63_f473,
    0x3f64_aa59,
    0x3f65_5e0b,
    0x3f66_0f88,
    0x3f66_becc,
    0x3f67_6bd8,
    0x3f68_16a8,
    0x3f68_bf3c,
    0x3f69_6591,
    0x3f6a_09a7,
    0x3f6a_ab7b,
    0x3f6b_4b0c,
    0x3f6b_e858,
    0x3f6c_835e,
    0x3f6d_1c1d,
    0x3f6d_b293,
    0x3f6e_46be,
    0x3f6e_d89e,
    0x3f6f_6830,
    0x3f6f_f573,
    0x3f70_8066,
    0x3f71_0908,
    0x3f71_8f57,
    0x3f72_1352,
    0x3f72_94f8,
    0x3f73_1447,
    0x3f73_913f,
    0x3f74_0bdd,
    0x3f74_8422,
    0x3f74_fa0b,
    0x3f75_6d97,
    0x3f75_dec6,
    0x3f76_4d97,
    0x3f76_ba07,
    0x3f77_2417,
    0x3f77_8bc5,
    0x3f77_f110,
    0x3f78_53f8,
    0x3f78_b47b,
    0x3f79_1298,
    0x3f79_6e4e,
    0x3f79_c79d,
    0x3f7a_1e84,
    0x3f7a_7302,
    0x3f7a_c516,
    0x3f7b_14be,
    0x3f7b_61fc,
    0x3f7b_accd,
    0x3f7b_f531,
    0x3f7c_3b28,
    0x3f7c_7eb0,
    0x3f7c_bfc9,
    0x3f7c_fe73,
    0x3f7d_3aac,
    0x3f7d_7474,
    0x3f7d_abcc,
    0x3f7d_e0b1,
    0x3f7e_1324,
    0x3f7e_4323,
    0x3f7e_70b0,
    0x3f7e_9bc9,
    0x3f7e_c46d,
    0x3f7e_ea9d,
    0x3f7f_0e58,
    0x3f7f_2f9d,
    0x3f7f_4e6d,
    0x3f7f_6ac7,
    0x3f7f_84ab,
    0x3f7f_9c18,
    0x3f7f_b10f,
    0x3f7f_c38f,
    0x3f7f_d397,
    0x3f7f_e129,
    0x3f7f_ec43,
    0x3f7f_f4e6,
    0x3f7f_fb11,
    0x3f7f_fec4,
    0x3f80_0000,
];

/// One sample of the quarter table.
fn sample(index: usize) -> f32 {
    f32::from_bits(SIN_QUARTER[index.min(QUARTER)])
}

/// Sine of `angle` radians, identically on every machine.
///
/// Folds the angle into one quarter turn and reads the table, which is what
/// keeps a single quarter's worth of samples enough for the whole circle.
///
/// A non-finite angle answers `0.0` rather than producing `NaN`: charter rule 4
/// forbids simulation from generating one at all, and the honest reading of
/// "which way is this pointing" for an angle that is not a number is "nowhere".
#[must_use]
pub fn sin(angle: f32) -> f32 {
    if !angle.is_finite() {
        return 0.0;
    }
    // Into `0..tau`. `rem_euclid` is a division and a multiply, both in the
    // allowed subset.
    let turn = angle.rem_euclid(TAU);
    #[expect(
        clippy::cast_precision_loss,
        reason = "QUARTER is 256 and exact in f32; this is a table position"
    )]
    let steps = turn / (TAU / 4.0) * QUARTER as f32;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`turn` is in `0..tau` so `steps` is in `0..4*QUARTER`, which fits"
    )]
    let whole = steps as usize;
    let fraction = steps - whole as f32;

    // Which quarter, and where in it. The second and fourth run backwards, and
    // the third and fourth are negative — the ordinary symmetry of a sine, done
    // with indices so that no branch changes the arithmetic.
    let quadrant = whole / QUARTER;
    let within = whole % QUARTER;
    let (low, high, sign) = match quadrant {
        0 => (sample(within), sample(within + 1), 1.0),
        1 => (sample(QUARTER - within), sample(QUARTER - within - 1), 1.0),
        2 => (sample(within), sample(within + 1), -1.0),
        _ => (sample(QUARTER - within), sample(QUARTER - within - 1), -1.0),
    };
    sign * (low + (high - low) * fraction)
}

/// Cosine of `angle` radians, identically on every machine.
///
/// A quarter turn ahead of the sine, which is the whole of it: one table, one
/// set of rounding, and no chance of the two disagreeing about a right angle.
#[must_use]
pub fn cos(angle: f32) -> f32 {
    sin(angle + TAU / 4.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How far from the real thing the table is allowed to be.
    ///
    /// Linear interpolation over a quarter turn in `QUARTER` steps: the bound
    /// is `|f''| h² / 8`, which is about 4.7e-6. A little room over that for
    /// the rounding of the samples themselves.
    const TOLERANCE: f32 = 1e-5;

    #[test]
    fn the_table_agrees_with_the_real_sine_everywhere() {
        // **The one place `f32::sin` is allowed**, and only to check the table
        // against it: a test may call libm because a test is not simulation.
        // What must never happen is the SHIPPED path calling it.
        #[expect(
            clippy::disallowed_methods,
            reason = "checking the committed table against the real function; not a sim path"
        )]
        for step in -2000..2000 {
            let angle = step as f32 * 0.0173;
            let (want, got) = (angle.sin(), sin(angle));
            assert!(
                (want - got).abs() < TOLERANCE,
                "sin({angle}) is {want} and the table says {got}"
            );
            let (want, got) = (angle.cos(), cos(angle));
            assert!(
                (want - got).abs() < TOLERANCE,
                "cos({angle}) is {want} and the table says {got}"
            );
        }
    }

    #[test]
    fn the_landmarks_are_exact_enough_to_build_on() {
        // The four angles a direction vector is actually built from, because a
        // facing that was 0.999 north would be a body pointing slightly wrong
        // in the one case anybody looks at.
        let quarter = TAU / 4.0;
        for (angle, s, c) in [
            (0.0, 0.0, 1.0),
            (quarter, 1.0, 0.0),
            (2.0 * quarter, 0.0, -1.0),
            (3.0 * quarter, -1.0, 0.0),
        ] {
            assert!((sin(angle) - s).abs() < TOLERANCE, "sin({angle})");
            assert!((cos(angle) - c).abs() < TOLERANCE, "cos({angle})");
        }
    }

    #[test]
    fn a_direction_built_from_it_is_a_unit_vector() {
        // What a caller actually does with these. `sin² + cos²` drifting from
        // one would be a throw that got faster as the player turned.
        for step in 0..720 {
            let angle = step as f32 * 0.0175;
            let (s, c) = (sin(angle), cos(angle));
            let length = s * s + c * c;
            assert!(
                (length - 1.0).abs() < 1e-4,
                "at {angle} the direction has length² {length}"
            );
        }
    }

    #[test]
    fn an_angle_that_is_not_a_number_is_not_answered_with_one() {
        // Charter rule 4: simulation must not produce NaN, and a caller feeding
        // one in must not be the way it does.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(sin(bad).to_bits(), 0.0f32.to_bits());
            assert!(cos(bad).is_finite());
        }
    }
}
