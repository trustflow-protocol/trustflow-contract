use soroban_sdk::{symbol_short, Address, Env, String};

use crate::errors::TrustFlowError;
use crate::storage::{extend_ttl, DataKey};
use crate::types::{DisputeRecord, EscrowRecord, EscrowStatus};

/// Raise a dispute on an escrow, halting release and calling for a jury.
///
/// Either party to the escrow (the depositor or the beneficiary) may raise a
/// dispute, and only while the escrow is `Active`. The escrow transitions to
/// `Disputed` (which blocks settlement) and a [`DisputeRecord`] is written for
/// the jury to resolve. A `disputed` event is emitted for indexers.
pub fn raise_dispute(
    env: &Env,
    escrow_id: u64,
    caller: &Address,
    reason: String,
) -> Result<(), TrustFlowError> {
    // The caller must have authorized this invocation.
    caller.require_auth();

    let mut escrow = env
        .storage()
        .persistent()
        .get::<DataKey, EscrowRecord>(&DataKey::Escrow(escrow_id))
        .ok_or(TrustFlowError::EscrowNotFound)?;

    // Only a party to the escrow may dispute it.
    if caller != &escrow.depositor && caller != &escrow.beneficiary {
        return Err(TrustFlowError::Unauthorized);
    }

    // Only an active escrow can be disputed. This also makes raising idempotent-
    // safe: a second call sees `Disputed` (not `Active`) and is rejected, and a
    // settled/cancelled escrow can never be re-opened by a dispute.
    if !matches!(escrow.status, EscrowStatus::Active) {
        return Err(TrustFlowError::InvalidState);
    }

    // Halt release.
    escrow.status = EscrowStatus::Disputed;
    env.storage()
        .persistent()
        .set(&DataKey::Escrow(escrow_id), &escrow);
    extend_ttl(env, &DataKey::Escrow(escrow_id));

    // Record the dispute for the jury.
    let dispute = DisputeRecord {
        escrow_id,
        raised_by: caller.clone(),
        reason,
        resolved: false,
        ruling_for_depositor: false,
    };
    env.storage()
        .persistent()
        .set(&DataKey::Dispute(escrow_id), &dispute);
    extend_ttl(env, &DataKey::Dispute(escrow_id));

    // Emit an event for indexers: topics (`disputed`, escrow_id), data raised_by.
    env.events()
        .publish((symbol_short!("disputed"), escrow_id), caller.clone());

    Ok(())
}
