use alloy::primitives::U256;
use std::fmt::{Display, Formatter};
use std::ops::{Add, Div, Mul};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct Gas(pub u64);

impl Display for Gas {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct FeePerGas(pub u128);

impl Display for FeePerGas {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct Pubdata(pub u64);

impl Display for Pubdata {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct GasPerPubdata(pub u64);

impl Display for GasPerPubdata {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct Wei(pub U256);

impl Display for Wei {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// ====================================================================
// Arithmetic operations for types above
// ====================================================================

impl Mul<Gas> for FeePerGas {
    type Output = Wei;

    fn mul(self, rhs: Gas) -> Self::Output {
        Wei(U256::from(self.0) * U256::from(rhs.0))
    }
}

impl Mul<FeePerGas> for Gas {
    type Output = Wei;

    fn mul(self, rhs: FeePerGas) -> Self::Output {
        Wei(U256::from(self.0) * U256::from(rhs.0))
    }
}

impl Add<FeePerGas> for FeePerGas {
    type Output = FeePerGas;

    fn add(self, rhs: FeePerGas) -> Self::Output {
        FeePerGas(self.0 + rhs.0)
    }
}

impl Mul<Pubdata> for GasPerPubdata {
    type Output = Gas;

    fn mul(self, rhs: Pubdata) -> Self::Output {
        Gas(self.0 * rhs.0)
    }
}

impl Mul<GasPerPubdata> for Pubdata {
    type Output = Gas;

    fn mul(self, rhs: GasPerPubdata) -> Self::Output {
        Gas(self.0 * rhs.0)
    }
}

impl Add<GasPerPubdata> for GasPerPubdata {
    type Output = GasPerPubdata;

    fn add(self, rhs: GasPerPubdata) -> Self::Output {
        GasPerPubdata(self.0 + rhs.0)
    }
}

impl Div<Gas> for Wei {
    type Output = FeePerGas;

    fn div(self, rhs: Gas) -> Self::Output {
        FeePerGas(
            self.0
                .div_ceil(U256::from(rhs.0))
                .try_into()
                .expect("gas price (`wei / gas`) overflow"),
        )
    }
}

impl Div<Pubdata> for Gas {
    type Output = GasPerPubdata;

    fn div(self, rhs: Pubdata) -> Self::Output {
        GasPerPubdata(self.0.div_ceil(rhs.0))
    }
}

impl Div<FeePerGas> for Wei {
    type Output = Gas;

    fn div(self, rhs: FeePerGas) -> Self::Output {
        Gas(self
            .0
            .div_ceil(U256::from(rhs.0))
            .try_into()
            .expect("gas (`wei / gas_price`) overflow"))
    }
}
