use alloy::primitives::U256;

#[derive(Debug, Clone, Copy)]
pub struct FeeParams {
    pub eip1559_basefee: U256,
    pub native_price: U256,
    pub pubdata_price: U256,
}
