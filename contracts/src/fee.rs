use crate::errors::TrustFlowError;

pub const DEFAULT_FEE_BPS: u32 = 50;
pub const MAX_FEE_BPS: u32 = 1_000;
pub const BASIS_POINTS: i128 = 10_000;

pub fn validate_fee_bps(fee_bps: u32) -> Result<(), TrustFlowError> {
    if fee_bps > MAX_FEE_BPS {
        Err(TrustFlowError::InvalidAmount)
    } else {
        Ok(())
    }
}

pub fn compute_fee(amount: i128, fee_bps: u32) -> Result<(i128, i128), TrustFlowError> {
    let fee = amount
        .checked_mul(fee_bps as i128)
        .and_then(|v| v.checked_div(BASIS_POINTS))
        .ok_or(TrustFlowError::ArithmeticOverflow)?;

    let payout = amount
        .checked_sub(fee)
        .ok_or(TrustFlowError::ArithmeticOverflow)?;

    Ok((fee, payout))
}
