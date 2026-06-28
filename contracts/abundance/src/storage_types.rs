use soroban_sdk::{contracttype, Address};

/// Approximate number of ledgers in a day (assuming ~5s per ledger)
pub(crate) const DAY_IN_LEDGERS: u32 = 17280;

/// Amount to bump instance storage TTL (7 days)
pub(crate) const INSTANCE_BUMP_AMOUNT: u32 = 7 * DAY_IN_LEDGERS;

/// Threshold for instance storage TTL extension (6 days)
pub(crate) const INSTANCE_LIFETIME_THRESHOLD: u32 = INSTANCE_BUMP_AMOUNT - DAY_IN_LEDGERS;

/// Amount to bump balance storage TTL (30 days)
pub(crate) const BALANCE_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;

/// Threshold for balance storage TTL extension (29 days)
pub(crate) const BALANCE_LIFETIME_THRESHOLD: u32 = BALANCE_BUMP_AMOUNT - DAY_IN_LEDGERS;

/// Key for allowance data, combining spender and owner addresses
#[derive(Clone)]
#[contracttype]
pub struct AllowanceDataKey {
    /// Address that owns the tokens
    pub from: Address,
    /// Address authorized to spend tokens
    pub spender: Address,
}

/// Value stored for an allowance, including amount and expiration
#[contracttype]
pub struct AllowanceValue {
    /// Maximum amount the spender can transfer
    pub amount: i128,
    /// Ledger number when this allowance expires
    pub expiration_ledger: u32,
}

/// Storage keys for the abundance token contract
#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    /// Allowance granted from one address to another
    Allowance(AllowanceDataKey),
    /// Token balance for an address
    Balance(Address),
    /// Nonce for replay protection
    Nonce(Address),
    /// General state data for an address
    State(Address),
    /// Contract administrator address
    Admin,
}
