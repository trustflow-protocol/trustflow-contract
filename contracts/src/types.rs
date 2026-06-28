use soroban_sdk::{contracttype, Address};

/// Status of an escrow transaction
///
/// Represents the current state in the escrow lifecycle:
/// - `Pending`: Escrow created but not yet active
/// - `Active`: Funds locked and escrow is active
/// - `Released`: Funds successfully released to beneficiary
/// - `Disputed`: A dispute has been raised and is under review
/// - `Cancelled`: Escrow cancelled and funds returned to depositor
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EscrowStatus {
    Pending,
    Active,
    Released,
    Disputed,
    Cancelled,
}

/// Record of an escrow transaction
///
/// Contains all information about an escrow agreement between
/// a depositor and beneficiary, including amounts, deadlines,
/// and current status.
#[contracttype]
#[derive(Clone, Debug)]
pub struct EscrowRecord {
    /// Unique identifier for this escrow
    pub id: u64,
    /// Address that deposited the funds
    pub depositor: Address,
    /// Address that will receive the funds upon release
    pub beneficiary: Address,
    /// Amount of tokens held in escrow
    pub amount: i128,
    /// Token contract address for the escrowed funds
    pub token: Address,
    /// Current status of the escrow
    pub status: EscrowStatus,
    /// Ledger timestamp when escrow was created
    pub created_at: u64,
    /// Ledger timestamp when funds can be released
    pub release_deadline: u64,
}

/// Record of a dispute raised against an escrow
///
/// When either party disputes an escrow, this record captures
/// the details for juror review and resolution.
#[contracttype]
#[derive(Clone, Debug)]
pub struct DisputeRecord {
    /// ID of the escrow being disputed
    pub escrow_id: u64,
    /// Address of the party who raised the dispute
    pub raised_by: Address,
    /// Explanation of why the dispute was raised
    pub reason: soroban_sdk::String,
    /// Whether the dispute has been resolved by jurors
    pub resolved: bool,
    /// If resolved, whether ruling favored depositor (true) or beneficiary (false)
    pub ruling_for_depositor: bool,
}
