use std::fmt;
use std::ops::{Add, Div, Mul, Neg, Rem, Sub};
use std::str::FromStr;

use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};

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
