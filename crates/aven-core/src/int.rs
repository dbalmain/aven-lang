use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, Div, Mul, Neg, Rem, Sub};
use std::str::FromStr;

use num_bigint::BigInt;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero};

/// Aven's arbitrary-precision integer representation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Int(BigInt);

impl Int {
    pub fn zero() -> Self {
        Self(BigInt::zero())
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    pub fn is_negative(&self) -> bool {
        self.0.is_negative()
    }

    pub fn abs(&self) -> Self {
        Self(self.0.abs())
    }

    pub fn signum(&self) -> Self {
        Self(self.0.signum())
    }

    pub fn pow(&self, exponent: u32) -> Self {
        Self(self.0.pow(exponent))
    }

    pub fn to_i32(&self) -> Option<i32> {
        self.0.to_i32()
    }

    pub fn to_i64(&self) -> Option<i64> {
        self.0.to_i64()
    }

    pub fn to_u8(&self) -> Option<u8> {
        self.0.to_u8()
    }

    pub fn to_u32(&self) -> Option<u32> {
        self.0.to_u32()
    }

    pub fn to_u64(&self) -> Option<u64> {
        self.0.to_u64()
    }

    pub fn to_usize(&self) -> Option<usize> {
        self.0.to_usize()
    }

    pub fn to_f64(&self) -> Option<f64> {
        self.0.to_f64()
    }

    /// The integer an `f64` holds exactly, or `None` when it has a fractional
    /// part or is not finite.
    pub fn from_f64_exact(value: f64) -> Option<Self> {
        if value.fract() == 0.0 {
            // `fract` is `NaN` for the non-finite floats, so this arm is
            // finite and converts without rounding.
            BigInt::from_f64(value).map(Self)
        } else {
            None
        }
    }
}

/// Exact comparison against `Float`, which is `f64`.
///
/// Narrowing the integer to `f64` first would collapse every integer past
/// 2^53 onto a shared float, which costs equality its transitivity: distinct
/// integers stay distinct while both compare equal to the same float. Every
/// hashed and ordered container depends on that transitivity, so the integer
/// side is never rounded — the float is split into its integer part, which
/// converts exactly, and the fraction that part leaves over.
impl PartialEq<f64> for Int {
    fn eq(&self, other: &f64) -> bool {
        self.partial_cmp(other) == Some(Ordering::Equal)
    }
}

impl PartialOrd<f64> for Int {
    /// `None` for `NaN` alone; every other float is ordered against every
    /// integer, infinities included.
    fn partial_cmp(&self, other: &f64) -> Option<Ordering> {
        if other.is_nan() {
            return None;
        }
        if other.is_infinite() {
            return Some(if other.is_sign_positive() {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }

        let whole = BigInt::from_f64(other.trunc())?;
        let fraction = other.fract();
        Some(match self.0.cmp(&whole) {
            // The integer parts match, so whatever the float carries beyond
            // its integer part decides.
            Ordering::Equal if fraction > 0.0 => Ordering::Less,
            Ordering::Equal if fraction < 0.0 => Ordering::Greater,
            ordering => ordering,
        })
    }
}

impl fmt::Display for Int {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Int {
    type Err = <BigInt as FromStr>::Err;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse().map(Self)
    }
}

macro_rules! impl_from_integer {
    ($($integer:ty),* $(,)?) => {
        $(
            impl From<$integer> for Int {
                fn from(value: $integer) -> Self {
                    Self(BigInt::from(value))
                }
            }
        )*
    };
}

impl_from_integer!(
    i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize
);

impl Add for &Int {
    type Output = Int;

    fn add(self, right: Self) -> Self::Output {
        Int(&self.0 + &right.0)
    }
}

impl Sub for &Int {
    type Output = Int;

    fn sub(self, right: Self) -> Self::Output {
        Int(&self.0 - &right.0)
    }
}

impl Mul for &Int {
    type Output = Int;

    fn mul(self, right: Self) -> Self::Output {
        Int(&self.0 * &right.0)
    }
}

impl Div for &Int {
    type Output = Int;

    fn div(self, right: Self) -> Self::Output {
        Int(&self.0 / &right.0)
    }
}

impl Rem for &Int {
    type Output = Int;

    fn rem(self, right: Self) -> Self::Output {
        Int(&self.0 % &right.0)
    }
}

impl Neg for &Int {
    type Output = Int;

    fn neg(self) -> Self::Output {
        Int(-&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::{Equal, Greater, Less};

    fn int(text: &str) -> Int {
        text.parse().expect("test integer literal is valid")
    }

    #[test]
    fn compares_integers_beyond_f64_precision_exactly() {
        // Both integers narrow to the same `f64`, so only an exact comparison
        // tells them apart.
        let float = 9_007_199_254_740_992.0_f64;
        assert_eq!(int("9007199254740992").partial_cmp(&float), Some(Equal));
        assert_eq!(int("9007199254740993").partial_cmp(&float), Some(Greater));
        assert_eq!(int("9007199254740991").partial_cmp(&float), Some(Less));

        let huge = 1.2345678901234568e29_f64;
        assert!(int("123456789012345678901234567890") != huge);
        assert!(int("123456789012345678901234567891") != huge);
        assert_eq!(
            Int::from_f64_exact(huge)
                .map(|value| value.to_string())
                .as_deref(),
            Some("123456789012345677877719597056"),
        );
    }

    #[test]
    fn orders_integers_against_fractions_by_the_leftover_fraction() {
        for (text, float, expected) in [
            ("1", 1.5, Less),
            ("2", 1.5, Greater),
            ("-1", -1.5, Greater),
            ("-2", -1.5, Less),
            ("-2", -2.5, Greater),
            ("0", 0.5, Less),
            ("0", -0.5, Greater),
        ] {
            assert_eq!(
                int(text).partial_cmp(&float),
                Some(expected),
                "{text} vs {float}"
            );
        }
    }

    #[test]
    fn treats_both_zeroes_as_the_integer_zero() {
        assert!(Int::zero() == 0.0);
        assert!(Int::zero() == -0.0);
        assert_eq!(Int::from_f64_exact(-0.0), Some(Int::zero()));
    }

    #[test]
    fn orders_against_infinities_and_declines_nan() {
        let big = int("123456789012345678901234567890");
        let nan = f64::NAN;
        assert_eq!(big.partial_cmp(&f64::INFINITY), Some(Less));
        assert_eq!(big.partial_cmp(&f64::NEG_INFINITY), Some(Greater));
        assert_eq!(big.partial_cmp(&nan), None);
        assert!(big != nan);
    }

    #[test]
    fn reads_only_whole_finite_floats_as_integers() {
        assert_eq!(Int::from_f64_exact(7.0), Some(Int::from(7)));
        assert_eq!(Int::from_f64_exact(-7.0), Some(Int::from(-7)));
        assert_eq!(Int::from_f64_exact(7.5), None);
        assert_eq!(Int::from_f64_exact(f64::INFINITY), None);
        assert_eq!(Int::from_f64_exact(f64::NAN), None);
    }
}
