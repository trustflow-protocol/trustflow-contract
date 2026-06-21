use soroban_sdk::{contracttype, Address};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EscrowStatus {
    Pending,
    Active,
    Released,
    Disputed,
    Cancelled,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct EscrowRecord {
    pub id: u64,
    pub depositor: Address,
    pub beneficiary: Address,
    pub amount: i128,
    pub token: Address,
    pub status: EscrowStatus,
    pub created_at: u64,
    pub release_deadline: u64,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct DisputeRecord {
    pub escrow_id: u64,
    pub raised_by: Address,
    pub reason: soroban_sdk::String,
    pub resolved: bool,
    pub ruling_for_depositor: bool,
}
