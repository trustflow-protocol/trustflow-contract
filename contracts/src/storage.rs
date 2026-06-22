use soroban_sdk::{contracttype, Address};

/// Storage keys for the TrustFlow escrow contract
///
/// Defines all persistent storage locations used by the contract
/// for tracking escrows, disputes, configuration, and state.
#[contracttype]
pub enum DataKey {
    /// Address of the contract administrator
    Admin,
    /// Counter for generating unique escrow IDs
    EscrowCounter,
    /// Escrow record keyed by escrow ID
    Escrow(u64),
    /// Dispute record keyed by escrow ID
    Dispute(u64),
    /// Fee charged in basis points (e.g., 100 = 1%)
    FeeBps,
    /// Address that receives collected protocol fees
    FeeCollector,
    /// Address of the oracle for external data
    OracleAddress,
    /// Whether the contract is paused (emergency stop)
    Paused,
}

/// Number of ledgers to extend persistent storage TTL (time-to-live)
/// ~30 days at 5s per ledger
pub const PERSISTENT_BUMP: u32 = 518_400;

/// Threshold at which to trigger TTL extension for persistent storage
/// ~7 days at 5s per ledger
pub const PERSISTENT_THRESHOLD: u32 = 120_960;

/// Extends the time-to-live of a storage entry to prevent expiration
///
/// # Arguments
/// * `env` - Contract environment
/// * `key` - Storage key to extend
pub fn extend_ttl(env: &soroban_sdk::Env, key: &DataKey) {
    if env.storage().persistent().has(key) {
        env.storage().persistent().extend_ttl(key, PERSISTENT_THRESHOLD, PERSISTENT_BUMP);
    }
}
