// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Sine, cosine and arc tangent that are the same number on every machine.
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
//! deterministic by construction"**. These are those tables.
//!
//! [`atan2`] is the inverse, and it was a KNOWN hole for a day: a reference mod
//! turned a direction into a yaw with Lua's `math.atan` — the platform's libm,
//! inside the tick, producing a yaw that is then persisted entity state. It was
//! written down in `docs/float-determinism.md` and beside the line rather than
//! quietly left, and this is what closes it.
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

/// `atan` over `0..=1`, as `f32` bit patterns.
///
/// **A quarter of the circle is all that is stored**, exactly as the sine's is:
/// every other octant is this one reflected, and doing it with one table means
/// one set of rounding rather than eight that could disagree at a boundary.
///
/// Committed rather than computed, for the reason at the top of this file.
static ATAN_UNIT: [u32; QUARTER + 1] = [
    0x0000_0000,
    0x3b7f_ffab,
    0x3bff_feab,
    0x3c3f_fdc0,
    0x3c7f_faab,
    0x3c9f_facb,
    0x3cbf_f701,
    0x3cdf_f1b7,
    0x3cff_eaae,
    0x3d0f_f0d3,
    0x3d1f_eb30,
    0x3d2f_e44d,
    0x3d3f_dc0c,
    0x3d4f_d24d,
    0x3d5f_c6f0,
    0x3d6f_b9d5,
    0x3d7f_aade,
    0x3d87_ccf5,
    0x3d8f_c36e,
    0x3d97_b8ca,
    0x3d9f_acf8,
    0x3da7_9feb,
    0x3daf_9192,
    0x3db7_81df,
    0x3dbf_70c1,
    0x3dc7_5e2a,
    0x3dcf_4a0b,
    0x3dd7_3454,
    0x3ddf_1cf6,
    0x3de7_03e3,
    0x3dee_e90c,
    0x3df6_cc61,
    0x3dfe_add5,
    0x3e03_46ac,
    0x3e07_356e,
    0x3e0b_232a,
    0x3e0f_0fd8,
    0x3e12_fb71,
    0x3e16_e5ee,
    0x3e1a_cf47,
    0x3e1e_b777,
    0x3e22_9e76,
    0x3e26_843d,
    0x3e2a_68c6,
    0x3e2e_4c09,
    0x3e32_2e00,
    0x3e36_0ea4,
    0x3e39_edef,
    0x3e3d_cbda,
    0x3e41_a85f,
    0x3e45_8377,
    0x3e49_5d1c,
    0x3e4d_3547,
    0x3e51_0bf3,
    0x3e54_e119,
    0x3e58_b4b3,
    0x3e5c_86bb,
    0x3e60_572a,
    0x3e64_25fc,
    0x3e67_f32a,
    0x3e6b_beaf,
    0x3e6f_8884,
    0x3e73_50a4,
    0x3e77_170a,
    0x3e7a_dbb0,
    0x3e7e_9e90,
    0x3e81_2fd3,
    0x3e83_0f75,
    0x3e84_ee2d,
    0x3e86_cbf7,
    0x3e88_a8d2,
    0x3e8a_84ba,
    0x3e8c_5fad,
    0x3e8e_39a9,
    0x3e90_12ab,
    0x3e91_eab1,
    0x3e93_c1b9,
    0x3e95_97c0,
    0x3e97_6cc4,
    0x3e99_40c2,
    0x3e9b_13ba,
    0x3e9c_e5a7,
    0x3e9e_b689,
    0x3ea0_865d,
    0x3ea2_5522,
    0x3ea4_22d4,
    0x3ea5_ef73,
    0x3ea7_bafc,
    0x3ea9_856d,
    0x3eab_4ec4,
    0x3ead_1701,
    0x3eae_de20,
    0x3eb0_a420,
    0x3eb2_6900,
    0x3eb4_2cbd,
    0x3eb5_ef56,
    0x3eb7_b0ca,
    0x3eb9_7117,
    0x3ebb_303b,
    0x3ebc_ee34,
    0x3ebe_ab02,
    0x3ec0_66a3,
    0x3ec2_2116,
    0x3ec3_da58,
    0x3ec5_926a,
    0x3ec7_4949,
    0x3ec8_fef4,
    0x3eca_b36a,
    0x3ecc_66aa,
    0x3ece_18b3,
    0x3ecf_c983,
    0x3ed1_791a,
    0x3ed3_2776,
    0x3ed4_d497,
    0x3ed6_807b,
    0x3ed8_2b21,
    0x3ed9_d489,
    0x3edb_7cb1,
    0x3edd_239a,
    0x3ede_c941,
    0x3ee0_6da6,
    0x3ee2_10c9,
    0x3ee3_b2a8,
    0x3ee5_5344,
    0x3ee6_f29a,
    0x3ee8_90ab,
    0x3eea_2d76,
    0x3eeb_c8fb,
    0x3eed_6338,
    0x3eee_fc2e,
    0x3ef0_93db,
    0x3ef2_2a40,
    0x3ef3_bf5c,
    0x3ef5_532e,
    0x3ef6_e5b7,
    0x3ef8_76f5,
    0x3efa_06e8,
    0x3efb_9591,
    0x3efd_22ef,
    0x3efe_af01,
    0x3f00_1ce4,
    0x3f00_e1a1,
    0x3f01_a5b8,
    0x3f02_692a,
    0x3f03_2bf5,
    0x3f03_ee1a,
    0x3f04_af98,
    0x3f05_7071,
    0x3f06_30a3,
    0x3f06_f02f,
    0x3f07_af14,
    0x3f08_6d54,
    0x3f09_2aed,
    0x3f09_e7e0,
    0x3f0a_a42d,
    0x3f0b_5fd3,
    0x3f0c_1ad4,
    0x3f0c_d52f,
    0x3f0d_8ee4,
    0x3f0e_47f4,
    0x3f0f_005d,
    0x3f0f_b822,
    0x3f10_6f41,
    0x3f11_25ba,
    0x3f11_db8f,
    0x3f12_90bf,
    0x3f13_454a,
    0x3f13_f931,
    0x3f14_ac73,
    0x3f15_5f11,
    0x3f16_110b,
    0x3f16_c261,
    0x3f17_7314,
    0x3f18_2324,
    0x3f18_d290,
    0x3f19_815a,
    0x3f1a_2f81,
    0x3f1a_dd06,
    0x3f1b_89e8,
    0x3f1c_3629,
    0x3f1c_e1c9,
    0x3f1d_8cc7,
    0x3f1e_3725,
    0x3f1e_e0e1,
    0x3f1f_89fe,
    0x3f20_327a,
    0x3f20_da57,
    0x3f21_8194,
    0x3f22_2833,
    0x3f22_ce33,
    0x3f23_7394,
    0x3f24_1857,
    0x3f24_bc7d,
    0x3f25_6006,
    0x3f26_02f1,
    0x3f26_a540,
    0x3f27_46f3,
    0x3f27_e80a,
    0x3f28_8885,
    0x3f29_2866,
    0x3f29_c7ac,
    0x3f2a_6658,
    0x3f2b_0469,
    0x3f2b_a1e2,
    0x3f2c_3ec1,
    0x3f2c_db08,
    0x3f2d_76b6,
    0x3f2e_11cd,
    0x3f2e_ac4c,
    0x3f2f_4635,
    0x3f2f_df87,
    0x3f30_7842,
    0x3f31_1069,
    0x3f31_a7fa,
    0x3f32_3ef6,
    0x3f32_d55e,
    0x3f33_6b32,
    0x3f34_0072,
    0x3f34_9520,
    0x3f35_293b,
    0x3f35_bcc5,
    0x3f36_4fbc,
    0x3f36_e223,
    0x3f37_73f9,
    0x3f38_053e,
    0x3f38_95f4,
    0x3f39_261b,
    0x3f39_b5b3,
    0x3f3a_44bc,
    0x3f3a_d338,
    0x3f3b_6127,
    0x3f3b_ee89,
    0x3f3c_7b5e,
    0x3f3d_07a7,
    0x3f3d_9365,
    0x3f3e_1e99,
    0x3f3e_a941,
    0x3f3f_3360,
    0x3f3f_bcf5,
    0x3f40_4602,
    0x3f40_ce86,
    0x3f41_5682,
    0x3f41_ddf6,
    0x3f42_64e4,
    0x3f42_eb4b,
    0x3f43_712c,
    0x3f43_f687,
    0x3f44_7b5e,
    0x3f44_ffb0,
    0x3f45_837e,
    0x3f46_06c9,
    0x3f46_8990,
    0x3f47_0bd5,
    0x3f47_8d98,
    0x3f48_0eda,
    0x3f48_8f9b,
    0x3f49_0fdb,
];

/// One sample of the arc-tangent table.
fn atan_sample(index: usize) -> f32 {
    f32::from_bits(ATAN_UNIT[index.min(QUARTER)])
}

/// `atan` of a ratio in `0..=1`, from the table.
fn atan_unit(ratio: f32) -> f32 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "QUARTER is 256 and exact in f32; this is a table position"
    )]
    let steps = ratio.clamp(0.0, 1.0) * QUARTER as f32;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`ratio` is clamped to `0..=1`, so `steps` is in `0..=QUARTER`"
    )]
    let whole = (steps as usize).min(QUARTER);
    let fraction = steps - whole as f32;
    let low = atan_sample(whole);
    let high = atan_sample(whole + 1);
    low + (high - low) * fraction
}

/// The angle of the vector `(x, y)` from the positive x axis, in radians.
///
/// The same contract as `f32::atan2` — the result is in `-pi..=pi` and the
/// signs of both arguments choose the quadrant — and the same number on every
/// machine, which `f32::atan2` is not.
///
/// # Why a mod needs this
///
/// Turning a direction back into a heading is what anything that points at
/// something does, and a heading is persisted entity state. A mod computing it
/// with Lua's `math.atan` is calling the platform's libm inside the tick, and
/// two servers running that mod diverge — quietly, in a saved world, in a way
/// no test on one machine can see.
///
/// # Folding
///
/// Only the first octant is stored, so the ratio handed to the table is always
/// the smaller of the two magnitudes over the larger. That keeps it in `0..=1`
/// where the samples are, and it keeps the accuracy even: the far end of a
/// `0..=inf` table would be sampling a function that has almost stopped moving.
///
/// Both arguments zero answers zero rather than being undefined: charter rule 4
/// forbids simulation from producing a NaN at all, and "which way is a vector of
/// no length pointing" has no better answer than "along x".
#[must_use]
pub fn atan2(y: f32, x: f32) -> f32 {
    if !y.is_finite() || !x.is_finite() {
        return 0.0;
    }
    let (ay, ax) = (y.abs(), x.abs());
    if ay == 0.0 && ax == 0.0 {
        return 0.0;
    }

    // The smaller over the larger, so the ratio lands in the table's range.
    // Whether it was swapped is what says which side of the diagonal we are on.
    let angle = if ay <= ax {
        atan_unit(ay / ax)
    } else {
        TAU / 4.0 - atan_unit(ax / ay)
    };

    // Out of the first octant and into the right quadrant. Written as
    // reflections rather than as four cases so that the arithmetic is the same
    // one everywhere and only the signs differ.
    let angle = if x < 0.0 { TAU / 2.0 - angle } else { angle };
    if y < 0.0 { -angle } else { angle }
}

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
    fn the_table_agrees_with_the_real_arc_tangent_everywhere() {
        // **The one place `f32::atan2` is allowed**, and only to check the
        // table against it. Every quadrant and both sides of the diagonal,
        // because the folding is where this can go wrong and a test that only
        // walked the first octant would prove the easy part.
        #[expect(
            clippy::disallowed_methods,
            reason = "checking the committed table against the real function; not a sim path"
        )]
        for iy in -40..=40 {
            for ix in -40..=40 {
                let (y, x) = (iy as f32 * 0.37, ix as f32 * 0.41);
                let (want, got) = (y.atan2(x), atan2(y, x));
                // Across the branch cut at pi the two are the same angle a
                // whole turn apart, which is not a disagreement. Written as
                // `diff.min((diff - TAU).abs())` and not as
                // `diff.abs().min(diff.abs() - TAU).abs()`, which was the first
                // version and reported every exact agreement as being a full
                // turn out.
                let diff = (want - got).abs();
                let apart = diff.min((diff - TAU).abs());
                assert!(
                    apart < TOLERANCE,
                    "atan2({y}, {x}) is {want} and the table says {got}"
                );
            }
        }
    }

    #[test]
    fn the_axes_and_the_diagonals_land_where_they_should() {
        // The eight directions anything actually points, because a mob facing
        // 0.999 north is the one case somebody looks at.
        let quarter = TAU / 4.0;
        let eighth = TAU / 8.0;
        for (y, x, want) in [
            (0.0, 1.0, 0.0),
            (1.0, 1.0, eighth),
            (1.0, 0.0, quarter),
            (1.0, -1.0, quarter + eighth),
            (0.0, -1.0, TAU / 2.0),
            (-1.0, -1.0, -(quarter + eighth)),
            (-1.0, 0.0, -quarter),
            (-1.0, 1.0, -eighth),
        ] {
            let got = atan2(y, x);
            assert!(
                (got - want).abs() < TOLERANCE,
                "atan2({y}, {x}) should be {want} and is {got}"
            );
        }
    }

    #[test]
    fn it_is_the_inverse_of_the_direction_the_sine_table_builds() {
        // The round trip a mod actually makes: a yaw becomes a facing through
        // `sin`/`cos`, and something pointing at a target turns a facing back
        // into a yaw. The two tables disagreeing would be a mob that drifted a
        // little every time it looked at you.
        for step in 0..512 {
            let angle = step as f32 * (TAU / 512.0) - TAU / 2.0;
            let back = atan2(sin(angle), cos(angle));
            let apart = (back - angle).abs();
            assert!(
                apart < 1e-3 || (apart - TAU).abs() < 1e-3,
                "{angle} became a direction and came back as {back}"
            );
        }
    }

    #[test]
    fn a_vector_of_no_length_points_along_x_rather_than_at_nothing() {
        // Charter rule 4 forbids the simulation producing a NaN at all, so the
        // undefined case has to answer something. `f32::atan2(0, 0)` is zero
        // too, which is the least surprising agreement to keep.
        assert!(atan2(0.0, 0.0).abs() < TOLERANCE);
        assert!(atan2(f32::NAN, 1.0).abs() < TOLERANCE);
        assert!(atan2(1.0, f32::INFINITY).abs() < TOLERANCE);
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
