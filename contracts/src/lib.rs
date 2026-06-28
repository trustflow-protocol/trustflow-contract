#![no_std]

mod dispute;
mod errors;
mod storage;
mod types;

#[cfg(test)]
mod test;

use soroban_sdk::{contract, contractimpl, Address, Env, String};

use crate::errors::TrustFlowError;

#[contract]
pub struct TrustFlowContract;

#[contractimpl]
impl TrustFlowContract {
    /// Raise a dispute on an active escrow.
    ///
    /// Callable by either party to the escrow (the depositor or the
    /// beneficiary). It halts release by moving the escrow to `Disputed` and
    /// records the dispute for a jury to resolve. Fails if the caller is not a
    /// party, the escrow does not exist, or it is not `Active`.
    pub fn raise_dispute(
        env: Env,
        escrow_id: u64,
        caller: Address,
        reason: String,
    ) -> Result<(), TrustFlowError> {
        dispute::raise_dispute(&env, escrow_id, &caller, reason)
    }
}
