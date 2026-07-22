#![cfg_attr(not(test), no_std)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
    IntoVal, String, Val, Vec,
};

/// Slash rate in basis points applied to minority voters (10% = 1000 bps)
const DEFAULT_SLASH_BPS: u32 = 1_000;

/// Default protocol fee charged on milestone tranche releases, in basis
/// points (50 = 0.50%). Snapshotted onto each escrow at creation time (see
/// [`EscrowFeeConfig`]) unless the admin has since changed the global
/// default via [`TrustFlow::set_fee_bps`].
const DEFAULT_FEE_BPS: u32 = 50;

/// Upper bound the admin may configure the global protocol fee to via
/// [`TrustFlow::set_fee_bps`] (1_000 bps = 10%).
const MAX_FEE_BPS: u32 = 1_000;

/// Denominator basis points are expressed against (10_000 bps = 100%).
const BPS_DENOMINATOR: i128 = 10_000;

// ---------------------------------------------------------------------------
// State archival / TTL bump strategy
// ---------------------------------------------------------------------------
//
// Soroban expires (archives) `persistent` and `instance` storage entries once
// their time-to-live (TTL) reaches zero. An archived entry cannot be read or
// written by a contract call again until it is restored via a separate
// `RestoreFootprintOp`, which requires the archived ledger key to still be
// known off-chain. Escrows in TrustFlow are frequently long-lived (milestone
// projects, disputes awaiting jurors) and may go untouched for far longer
// than a persistent entry's default TTL, so every persistent write below is
// paired with an immediate TTL bump, and `bump_escrow_ttl` is exposed so
// anyone (e.g. a keeper/cron job) can keep a dormant escrow alive without
// needing to mutate its state.
//
// One ledger closes roughly every 5 seconds.
const LEDGERS_PER_DAY: u32 = 17_280;

/// Escrow/dispute/juror persistent records — and the contract instance
/// itself (admin/token/slash config/escrow counter) — are bumped to live 90
/// days out once they have less than 30 days of TTL remaining. Escrows are
/// expected to significantly outlive the SDK's default entry TTL, and
/// `bump_escrow_ttl` is the sole heartbeat a fully dormant escrow gets.
///
/// The instance and persistent watermarks are intentionally identical:
/// every contract call — `bump_escrow_ttl` included — needs the *instance*
/// to still be live just to be invoked at all, so if instance TTL were ever
/// allowed to lapse before an escrow's persistent TTL, the whole contract
/// would archive first and no one could call `bump_escrow_ttl` to save it.
/// Keeping both on the same clock means one keeper call refreshes both.
///
/// 90/30 are conservative round numbers, not derived from a formula against
/// expected ledger close times — they're a deliberately generous starting
/// point (escrows are the whole point of this contract, so we'd rather
/// over-provision TTL than under-provision it) and a reasonable target for a
/// keeper to poll on a monthly-ish cadence. Revisit if real-world keeper
/// cadence or rent costs suggest a tighter window.
const BUMP_AMOUNT: u32 = 90 * LEDGERS_PER_DAY;
const LIFETIME_THRESHOLD: u32 = 30 * LEDGERS_PER_DAY;
const INSTANCE_BUMP_AMOUNT: u32 = BUMP_AMOUNT;
const INSTANCE_LIFETIME_THRESHOLD: u32 = LIFETIME_THRESHOLD;
const PERSISTENT_BUMP_AMOUNT: u32 = BUMP_AMOUNT;
const PERSISTENT_LIFETIME_THRESHOLD: u32 = LIFETIME_THRESHOLD;

/// Extends the current contract instance's TTL. Cheap to call repeatedly:
/// the host only writes a new TTL when the instance is within
/// [`INSTANCE_LIFETIME_THRESHOLD`] ledgers of expiring.
fn extend_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .bump(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
}

/// Extends a persistent storage entry's TTL. No-op if the entry does not
/// exist, and cheap to call repeatedly for the same reason as above.
fn extend_persistent_ttl<K>(env: &Env, key: &K)
where
    K: IntoVal<Env, Val>,
{
    if env.storage().persistent().has(key) {
        env.storage()
            .persistent()
            .bump(key, PERSISTENT_LIFETIME_THRESHOLD, PERSISTENT_BUMP_AMOUNT);
    }
}

/// Computes `floor(amount * fee_bps / 10_000)` without ever forming the
/// (potentially overflowing) product `amount * fee_bps` directly.
///
/// `amount` is decomposed as `quotient * 10_000 + remainder` (via integer
/// div/mod), so `amount * fee_bps / 10_000 == quotient * fee_bps +
/// remainder * fee_bps / 10_000`, and `remainder < 10_000` keeps the second
/// multiplication small regardless of how large `amount` is. Every step is
/// checked so an impossible overflow is reported as an error rather than
/// silently wrapping.
///
/// Used to charge each milestone tranche exactly
/// `cumulative_fee(released_after) - cumulative_fee(released_before)`, which
/// guarantees fragmenting one release into many never changes the total fee
/// collected.
fn cumulative_fee(amount: i128, fee_bps: u32) -> Result<i128, TrustFlowError> {
    if amount == 0 {
        return Ok(0);
    }
    let fee_bps = fee_bps as i128;
    let quotient = amount / BPS_DENOMINATOR;
    let remainder = amount % BPS_DENOMINATOR;

    let quotient_fee = quotient
        .checked_mul(fee_bps)
        .ok_or(TrustFlowError::InvalidAmount)?;
    let remainder_fee = remainder
        .checked_mul(fee_bps)
        .ok_or(TrustFlowError::InvalidAmount)?
        / BPS_DENOMINATOR;

    quotient_fee
        .checked_add(remainder_fee)
        .ok_or(TrustFlowError::InvalidAmount)
}

/// Snapshots the current global `FeeBps`/`Treasury` defaults onto a
/// newly-created escrow. Called once, right after the escrow record itself
/// is persisted, from both `create_escrow` and `init_escrow`. Once written
/// this snapshot is never mutated by later `set_fee_bps` calls.
fn snapshot_fee_config(env: &Env, escrow_id: u64) {
    let fee_bps: u32 = env
        .storage()
        .instance()
        .get(&DataKey::FeeBps)
        .unwrap_or(DEFAULT_FEE_BPS);
    let treasury: Address = env.storage().instance().get(&DataKey::Treasury).unwrap();

    let fee_config_key = DataKey::EscrowFeeConfig(escrow_id);
    env.storage()
        .persistent()
        .set(&fee_config_key, &EscrowFeeConfig { treasury, fee_bps });
    extend_persistent_ttl(env, &fee_config_key);
}

fn try_token_transfer(
    env: &Env,
    token_address: &Address,
    from: &Address,
    to: &Address,
    amount: i128,
) -> Result<(), TrustFlowError> {
    if amount == 0 {
        return Ok(());
    }

    let token_client = token::Client::new(env, token_address);
    match token_client.try_transfer(from, to, &amount) {
        Ok(Ok(())) => Ok(()),
        _ => Err(TrustFlowError::TokenTransferFailed),
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TrustFlowError {
    Unauthorized = 1,
    EscrowNotFound = 2,
    InvalidAmount = 3,
    DisputeNotFound = 4,
    DisputeAlreadyResolved = 5,
    InvalidState = 6,
    AlreadyVoted = 7,
    InsufficientStake = 8,
    NoVotesCast = 9,
    MilestoneAmountMismatch = 10,
    JurorStakeNotFound = 11,
    MilestoneNotFound = 12,
    InvalidFeeBps = 13,
    ArithmeticOverflow = 14,
    TokenTransferFailed = 15,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug)]
pub struct EscrowInitialized {
    pub escrow_id: u64,
    pub depositor: Address,
    pub beneficiary: Address,
    pub amount: i128,
}

/// Emitted by [`TrustFlow::bump_escrow_ttl`]. `live_until_ledger` is the
/// watermark the bump was requested up to — the host only actually rewrites
/// the TTL if the entry was within [`PERSISTENT_LIFETIME_THRESHOLD`] ledgers
/// of expiring, so the entry's real expiration may already have been further
/// out than this value if it was bumped recently.
#[contracttype]
#[derive(Clone, Debug)]
pub struct EscrowTtlBumped {
    pub escrow_id: u64,
    pub live_until_ledger: u32,
}

/// Emitted by [`TrustFlow::bump_juror_stake_ttl`]. Same caveat on
/// `live_until_ledger` as [`EscrowTtlBumped`].
#[contracttype]
#[derive(Clone, Debug)]
pub struct JurorStakeTtlBumped {
    pub juror: Address,
    pub live_until_ledger: u32,
}

/// Emitted by [`TrustFlow::release_milestone_tranche`]. `escrow_id` and
/// `milestone_index` are carried as indexed topics (not duplicated in the
/// data payload) so indexers can filter without decoding every event.
#[contracttype]
#[derive(Clone, Debug)]
pub struct MilestoneTrancheReleased {
    pub gross_amount: i128,
    pub treasury_fee: i128,
    pub beneficiary_payout: i128,
    pub milestone_released: i128,
    pub escrow_released: i128,
    pub beneficiary: Address,
    pub treasury: Address,
}

// ---------------------------------------------------------------------------
// Storage types
// ---------------------------------------------------------------------------

/// Composite key used to store a single juror's vote for a specific dispute.
/// A separate struct is needed because contracttype enums only support single-element
/// tuple variants for storage keys.
#[contracttype]
#[derive(Clone, Debug)]
pub struct VoteKey {
    pub escrow_id: u64,
    pub juror: Address,
}

/// Composite key identifying a single milestone within an escrow. A separate
/// struct is needed (rather than a `(u64, u32)` tuple variant) because
/// contracttype enums only support single-element tuple variants for storage
/// keys — mirrors the [`VoteKey`] pattern above.
#[contracttype]
#[derive(Clone, Debug)]
pub struct MilestoneKey {
    pub escrow_id: u64,
    pub milestone_index: u32,
}

#[contracttype]
pub enum DataKey {
    /// Contract administrator
    Admin,
    /// Address of the stake/settlement token
    Token,
    /// Slash rate in basis points
    SlashBps,
    /// Counter for generating unique escrow IDs
    EscrowCounter,
    /// Global default protocol fee in basis points, admin-configurable via
    /// `set_fee_bps`. Snapshotted per-escrow at creation time; changing this
    /// never affects escrows created before the change.
    FeeBps,
    /// Global default treasury address fees are paid to. Snapshotted
    /// per-escrow at creation time alongside `FeeBps`.
    Treasury,
    /// EscrowRecord keyed by escrow ID
    Escrow(u64),
    /// Snapshotted fee configuration (treasury + fee_bps) for an escrow,
    /// captured at creation time and immutable thereafter.
    EscrowFeeConfig(u64),
    /// Cumulative gross amount released across all milestone tranches for an
    /// escrow (i128)
    EscrowReleased(u64),
    /// Cumulative gross amount released for a single milestone within an
    /// escrow (i128)
    MilestoneReleased(MilestoneKey),
    /// DisputeRecord keyed by escrow ID
    Dispute(u64),
    /// Staked token balance for a juror (i128)
    JurorStake(Address),
    /// Ordered list of jurors who voted on a dispute (Vec<Address>)
    DisputeVoters(u64),
    /// A juror's vote direction: true = for depositor, false = for beneficiary
    JurorVote(VoteKey),
    /// How many times a juror has been slashed (u32)
    JurorSlashCount(Address),
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EscrowStatus {
    Active,
    Disputed,
    Settled,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub label: String,
    pub amount: i128,
    pub approved: bool,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct EscrowRecord {
    pub id: u64,
    pub depositor: Address,
    pub beneficiary: Address,
    pub amount: i128,
    pub status: EscrowStatus,
    pub milestones: Vec<Milestone>,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct DisputeRecord {
    pub escrow_id: u64,
    pub raised_by: Address,
    pub reason: String,
    pub resolved: bool,
    pub ruling_for_depositor: bool,
}

/// Fee configuration snapshotted onto an escrow at creation time. Later
/// changes to the global `FeeBps`/`Treasury` defaults (via `set_fee_bps`)
/// never retroactively alter an already-created escrow's snapshot.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EscrowFeeConfig {
    pub treasury: Address,
    pub fee_bps: u32,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct TrustFlow;

#[contractimpl]
impl TrustFlow {
    // -----------------------------------------------------------------------
    // Initialisation
    // -----------------------------------------------------------------------

    /// Initialise the contract. Must be called once before any other function.
    /// `slash_bps` are deducted from a minority voter's stake on each slash.
    pub fn initialize(env: Env, admin: Address, token: Address, slash_bps: u32) {
        admin.require_auth();
        if slash_bps > 10_000 {
            panic!("slash_bps must be <= 10_000");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::SlashBps, &slash_bps);
        env.storage().instance().set(&DataKey::EscrowCounter, &0u64);
        // Default protocol fee is 50 bps, paid to the admin's address until
        // `set_treasury` is used to point it elsewhere.
        env.storage()
            .instance()
            .set(&DataKey::FeeBps, &DEFAULT_FEE_BPS);
        env.storage().instance().set(&DataKey::Treasury, &admin);
        extend_instance_ttl(&env);
    }

    /// Admin-only: update the global default protocol fee (bps) used to
    /// snapshot new escrows. Does not affect already-created escrows.
    pub fn set_fee_bps(env: Env, caller: Address, fee_bps: u32) -> Result<(), TrustFlowError> {
        caller.require_auth();
        extend_instance_ttl(&env);
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if caller != admin {
            return Err(TrustFlowError::Unauthorized);
        }
        if fee_bps > MAX_FEE_BPS {
            return Err(TrustFlowError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        Ok(())
    }

    /// Return the current global default protocol fee, in basis points.
    pub fn get_fee_bps(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::FeeBps)
            .unwrap_or(DEFAULT_FEE_BPS)
    }

    /// Return the current global default treasury address.
    pub fn get_treasury(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Treasury).unwrap()
    }

    /// Admin-only: update the global default treasury address used to
    /// snapshot new escrows. Does not affect already-created escrows.
    pub fn set_treasury(
        env: Env,
        caller: Address,
        treasury: Address,
    ) -> Result<(), TrustFlowError> {
        caller.require_auth();
        extend_instance_ttl(&env);
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if caller != admin {
            return Err(TrustFlowError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Juror staking
    // -----------------------------------------------------------------------

    /// Stake `amount` tokens.  Transfers tokens from `juror` to this contract.
    pub fn stake(env: Env, juror: Address, amount: i128) -> Result<(), TrustFlowError> {
        juror.require_auth();
        extend_instance_ttl(&env);
        if amount <= 0 {
            return Err(TrustFlowError::InvalidAmount);
        }
        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        token::Client::new(&env, &token).transfer(&juror, &env.current_contract_address(), &amount);

        let key = DataKey::JurorStake(juror.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + amount));
        extend_persistent_ttl(&env, &key);
        Ok(())
    }

    /// Withdraw `amount` tokens that have not been slashed.
    pub fn unstake(env: Env, juror: Address, amount: i128) -> Result<(), TrustFlowError> {
        juror.require_auth();
        extend_instance_ttl(&env);
        let key = DataKey::JurorStake(juror.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if current < amount {
            return Err(TrustFlowError::InsufficientStake);
        }
        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &juror, &amount);
        env.storage().persistent().set(&key, &(current - amount));
        extend_persistent_ttl(&env, &key);
        Ok(())
    }

    /// Return the current staked balance for `juror`.
    pub fn get_stake(env: Env, juror: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::JurorStake(juror))
            .unwrap_or(0)
    }

    /// Return how many times `juror` has been slashed.
    pub fn get_slash_count(env: Env, juror: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::JurorSlashCount(juror))
            .unwrap_or(0)
    }

    /// Extend the TTL of a juror's stake/slash-count storage. Callable by
    /// anyone. Errors if the juror has no stake on record.
    pub fn bump_juror_stake_ttl(env: Env, juror: Address) -> Result<u32, TrustFlowError> {
        // A juror may stay staked for a long time between disputes; without
        // periodic maintenance their stake entry could archive and, on the
        // next dispute they are drawn into, `resolve_dispute` would be
        // unable to read or slash it. Returns the ledger sequence up to
        // which the stake is now guaranteed to live.
        let stake_key = DataKey::JurorStake(juror.clone());
        if !env.storage().persistent().has(&stake_key) {
            return Err(TrustFlowError::JurorStakeNotFound);
        }

        extend_instance_ttl(&env);
        extend_persistent_ttl(&env, &stake_key);
        extend_persistent_ttl(&env, &DataKey::JurorSlashCount(juror.clone()));

        let live_until_ledger = env
            .ledger()
            .sequence()
            .saturating_add(PERSISTENT_BUMP_AMOUNT);
        env.events().publish(
            (symbol_short!("juror"), symbol_short!("ttlbump")),
            JurorStakeTtlBumped {
                juror,
                live_until_ledger,
            },
        );

        Ok(live_until_ledger)
    }

    // -----------------------------------------------------------------------
    // Escrow lifecycle
    // -----------------------------------------------------------------------

    /// Create an escrow and lock `amount` tokens from `depositor`.
    /// Returns the new escrow ID.
    pub fn create_escrow(
        env: Env,
        depositor: Address,
        beneficiary: Address,
        amount: i128,
    ) -> Result<u64, TrustFlowError> {
        depositor.require_auth();
        extend_instance_ttl(&env);
        if amount <= 0 {
            return Err(TrustFlowError::InvalidAmount);
        }
        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        token::Client::new(&env, &token).transfer(
            &depositor,
            &env.current_contract_address(),
            &amount,
        );

        let counter: u64 = env
            .storage()
            .instance()
            .get(&DataKey::EscrowCounter)
            .unwrap_or(0);
        let id = counter + 1;
        env.storage().instance().set(&DataKey::EscrowCounter, &id);

        let escrow_key = DataKey::Escrow(id);
        env.storage().persistent().set(
            &escrow_key,
            &EscrowRecord {
                id,
                depositor,
                beneficiary,
                amount,
                status: EscrowStatus::Active,
                milestones: Vec::new(&env),
            },
        );
        extend_persistent_ttl(&env, &escrow_key);
        snapshot_fee_config(&env, id);
        Ok(id)
    }

    /// Initialise an escrow with specific milestones and lock `amount` tokens.
    pub fn init_escrow(
        env: Env,
        depositor: Address,
        beneficiary: Address,
        milestones: Vec<Milestone>,
    ) -> Result<u64, TrustFlowError> {
        depositor.require_auth();
        extend_instance_ttl(&env);

        let mut total_amount: i128 = 0;
        for m in milestones.iter() {
            if m.amount <= 0 {
                return Err(TrustFlowError::InvalidAmount);
            }
            total_amount += m.amount;
        }

        if total_amount <= 0 {
            return Err(TrustFlowError::InvalidAmount);
        }

        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        token::Client::new(&env, &token).transfer(
            &depositor,
            &env.current_contract_address(),
            &total_amount,
        );

        let counter: u64 = env
            .storage()
            .instance()
            .get(&DataKey::EscrowCounter)
            .unwrap_or(0);
        let id = counter + 1;
        env.storage().instance().set(&DataKey::EscrowCounter, &id);

        let escrow_key = DataKey::Escrow(id);
        env.storage().persistent().set(
            &escrow_key,
            &EscrowRecord {
                id,
                depositor: depositor.clone(),
                beneficiary: beneficiary.clone(),
                amount: total_amount,
                status: EscrowStatus::Active,
                milestones,
            },
        );
        extend_persistent_ttl(&env, &escrow_key);
        snapshot_fee_config(&env, id);

        env.events().publish(
            (symbol_short!("escrow"), symbol_short!("init")),
            EscrowInitialized {
                escrow_id: id,
                depositor,
                beneficiary,
                amount: total_amount,
            },
        );

        Ok(id)
    }

    /// Release a `gross_amount` tranche of a milestone to its beneficiary,
    /// splitting the escrow's snapshotted fee to its snapshotted treasury
    /// atomically. Caller must be the depositor. May be called repeatedly
    /// per milestone; settles the escrow once fully released.
    pub fn release_milestone_tranche(
        env: Env,
        escrow_id: u64,
        milestone_index: u32,
        gross_amount: i128,
        caller: Address,
    ) -> Result<(), TrustFlowError> {
        // Each tranche is charged exactly
        // cumulative_fee(escrow_released_after) - cumulative_fee(escrow_released_before),
        // which guarantees splitting one release into many tranches never
        // changes the total fee collected across the whole escrow (see
        // `cumulative_fee`). All validation and storage accounting happens
        // before either token transfer, and both transfers happen in this
        // same invocation -- if either fails, Soroban rolls back the whole
        // call, so there is no way to end up with mismatched books.
        caller.require_auth();
        extend_instance_ttl(&env);

        if gross_amount <= 0 {
            return Err(TrustFlowError::InvalidAmount);
        }

        let escrow_key = DataKey::Escrow(escrow_id);
        let mut escrow: EscrowRecord = env
            .storage()
            .persistent()
            .get(&escrow_key)
            .ok_or(TrustFlowError::EscrowNotFound)?;

        if caller != escrow.depositor {
            return Err(TrustFlowError::Unauthorized);
        }
        if escrow.status != EscrowStatus::Active {
            return Err(TrustFlowError::InvalidState);
        }

        let mut milestone = escrow
            .milestones
            .get(milestone_index)
            .ok_or(TrustFlowError::MilestoneNotFound)?;

        let milestone_key = DataKey::MilestoneReleased(MilestoneKey {
            escrow_id,
            milestone_index,
        });
        let milestone_released_before: i128 =
            env.storage().persistent().get(&milestone_key).unwrap_or(0);
        let milestone_released_after = milestone_released_before
            .checked_add(gross_amount)
            .ok_or(TrustFlowError::InvalidAmount)?;
        if milestone_released_after > milestone.amount {
            return Err(TrustFlowError::MilestoneAmountMismatch);
        }

        let released_key = DataKey::EscrowReleased(escrow_id);
        let escrow_released_before: i128 =
            env.storage().persistent().get(&released_key).unwrap_or(0);
        let escrow_released_after = escrow_released_before
            .checked_add(gross_amount)
            .ok_or(TrustFlowError::InvalidAmount)?;
        if escrow_released_after > escrow.amount {
            return Err(TrustFlowError::MilestoneAmountMismatch);
        }

        let fee_config_key = DataKey::EscrowFeeConfig(escrow_id);
        let fee_config: EscrowFeeConfig = env
            .storage()
            .persistent()
            .get(&fee_config_key)
            .ok_or(TrustFlowError::EscrowNotFound)?;
        extend_persistent_ttl(&env, &fee_config_key);

        let fee_before = cumulative_fee(escrow_released_before, fee_config.fee_bps)?;
        let fee_after = cumulative_fee(escrow_released_after, fee_config.fee_bps)?;
        let treasury_fee = fee_after
            .checked_sub(fee_before)
            .ok_or(TrustFlowError::InvalidAmount)?;
        let beneficiary_payout = gross_amount
            .checked_sub(treasury_fee)
            .ok_or(TrustFlowError::InvalidAmount)?;

        // -- All validation done; persist accounting before any transfer. --

        milestone.approved = true;
        escrow.milestones.set(milestone_index, milestone);
        if escrow_released_after == escrow.amount {
            escrow.status = EscrowStatus::Settled;
        }
        env.storage().persistent().set(&escrow_key, &escrow);
        extend_persistent_ttl(&env, &escrow_key);

        env.storage()
            .persistent()
            .set(&milestone_key, &milestone_released_after);
        extend_persistent_ttl(&env, &milestone_key);

        env.storage()
            .persistent()
            .set(&released_key, &escrow_released_after);
        extend_persistent_ttl(&env, &released_key);

        // -- Accounting is durable; now move the tokens. --

        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let contract_address = env.current_contract_address();
        try_token_transfer(
            &env,
            &token,
            &contract_address,
            &escrow.beneficiary,
            beneficiary_payout,
        )?;
        try_token_transfer(
            &env,
            &token,
            &contract_address,
            &fee_config.treasury,
            treasury_fee,
        )?;

        env.events().publish(
            (
                symbol_short!("mstone"),
                symbol_short!("release"),
                escrow_id,
                milestone_index,
            ),
            MilestoneTrancheReleased {
                gross_amount,
                treasury_fee,
                beneficiary_payout,
                milestone_released: milestone_released_after,
                escrow_released: escrow_released_after,
                beneficiary: escrow.beneficiary,
                treasury: fee_config.treasury,
            },
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Dispute management
    // -----------------------------------------------------------------------

    /// Raise a dispute on an active escrow.  Only the depositor or beneficiary
    /// may call this.
    pub fn raise_dispute(
        env: Env,
        escrow_id: u64,
        caller: Address,
        reason: String,
    ) -> Result<(), TrustFlowError> {
        caller.require_auth();
        extend_instance_ttl(&env);
        let escrow_key = DataKey::Escrow(escrow_id);
        let mut escrow: EscrowRecord = env
            .storage()
            .persistent()
            .get(&escrow_key)
            .ok_or(TrustFlowError::EscrowNotFound)?;

        if escrow.status != EscrowStatus::Active {
            return Err(TrustFlowError::InvalidState);
        }
        if caller != escrow.depositor && caller != escrow.beneficiary {
            return Err(TrustFlowError::Unauthorized);
        }

        escrow.status = EscrowStatus::Disputed;
        env.storage().persistent().set(&escrow_key, &escrow);
        extend_persistent_ttl(&env, &escrow_key);

        // Dispute settlement depends on the cumulative released amount.
        // Keep it alive whenever the escrow is refreshed on dispute entry.
        extend_persistent_ttl(&env, &DataKey::EscrowReleased(escrow_id));

        let dispute_key = DataKey::Dispute(escrow_id);
        env.storage().persistent().set(
            &dispute_key,
            &DisputeRecord {
                escrow_id,
                raised_by: caller,
                reason,
                resolved: false,
                ruling_for_depositor: false,
            },
        );
        extend_persistent_ttl(&env, &dispute_key);
        Ok(())
    }

    /// Cast a vote on an open dispute.  The calling juror must have a positive
    /// stake balance before voting.
    ///
    /// * `vote_for_depositor` – `true` rules in favour of the depositor;
    ///   `false` rules in favour of the beneficiary.
    pub fn cast_vote(
        env: Env,
        escrow_id: u64,
        juror: Address,
        vote_for_depositor: bool,
    ) -> Result<(), TrustFlowError> {
        juror.require_auth();
        extend_instance_ttl(&env);

        let stake: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::JurorStake(juror.clone()))
            .unwrap_or(0);
        if stake <= 0 {
            return Err(TrustFlowError::InsufficientStake);
        }

        let dispute_key = DataKey::Dispute(escrow_id);
        let dispute: DisputeRecord = env
            .storage()
            .persistent()
            .get(&dispute_key)
            .ok_or(TrustFlowError::DisputeNotFound)?;
        if dispute.resolved {
            return Err(TrustFlowError::DisputeAlreadyResolved);
        }

        let vote_key = DataKey::JurorVote(VoteKey {
            escrow_id,
            juror: juror.clone(),
        });
        if env.storage().persistent().has(&vote_key) {
            return Err(TrustFlowError::AlreadyVoted);
        }

        env.storage()
            .persistent()
            .set(&vote_key, &vote_for_depositor);
        extend_persistent_ttl(&env, &vote_key);

        let voters_key = DataKey::DisputeVoters(escrow_id);
        let mut voters: Vec<Address> = env
            .storage()
            .persistent()
            .get(&voters_key)
            .unwrap_or_else(|| Vec::new(&env));
        voters.push_back(juror);
        env.storage().persistent().set(&voters_key, &voters);
        extend_persistent_ttl(&env, &voters_key);
        extend_persistent_ttl(&env, &dispute_key);

        Ok(())
    }

    /// Resolve a dispute by tallying juror votes.
    ///
    /// Jurors whose vote disagrees with the majority ruling are **slashed**:
    /// `slash_bps / 10_000` of their current stake is burned from their
    /// in-contract balance.  In the event of a tie the ruling favours the
    /// depositor.
    ///
    /// Returns `true` if the ruling is for the depositor, `false` otherwise.
    pub fn resolve_dispute(env: Env, escrow_id: u64) -> Result<bool, TrustFlowError> {
        extend_instance_ttl(&env);
        let dispute_key = DataKey::Dispute(escrow_id);
        let mut dispute: DisputeRecord = env
            .storage()
            .persistent()
            .get(&dispute_key)
            .ok_or(TrustFlowError::DisputeNotFound)?;
        if dispute.resolved {
            return Err(TrustFlowError::DisputeAlreadyResolved);
        }

        let voters: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::DisputeVoters(escrow_id))
            .unwrap_or_else(|| Vec::new(&env));
        if voters.is_empty() {
            return Err(TrustFlowError::NoVotesCast);
        }

        // Tally
        let mut for_depositor: u32 = 0;
        let mut for_beneficiary: u32 = 0;
        for voter in voters.iter() {
            let vote: bool = env
                .storage()
                .persistent()
                .get(&DataKey::JurorVote(VoteKey {
                    escrow_id,
                    juror: voter.clone(),
                }))
                .unwrap_or(false);
            if vote {
                for_depositor += 1;
            } else {
                for_beneficiary += 1;
            }
        }

        // Tie breaks in favour of the depositor
        let ruling = for_depositor >= for_beneficiary;

        // Slash minority voters
        let slash_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::SlashBps)
            .unwrap_or(DEFAULT_SLASH_BPS);

        for voter in voters.iter() {
            let vote: bool = env
                .storage()
                .persistent()
                .get(&DataKey::JurorVote(VoteKey {
                    escrow_id,
                    juror: voter.clone(),
                }))
                .unwrap_or(false);

            if vote != ruling {
                let stake_key = DataKey::JurorStake(voter.clone());
                let stake: i128 = env.storage().persistent().get(&stake_key).unwrap_or(0);
                // slash_amount = stake * slash_bps / 10_000, saturating at stake
                let slash = stake
                    .checked_mul(slash_bps as i128)
                    .unwrap_or(stake * DEFAULT_SLASH_BPS as i128)
                    .checked_div(10_000)
                    .unwrap_or(0)
                    .min(stake);
                env.storage().persistent().set(&stake_key, &(stake - slash));
                extend_persistent_ttl(&env, &stake_key);

                let slash_count_key = DataKey::JurorSlashCount(voter.clone());
                let count: u32 = env
                    .storage()
                    .persistent()
                    .get(&slash_count_key)
                    .unwrap_or(0);
                env.storage()
                    .persistent()
                    .set(&slash_count_key, &(count + 1));
                extend_persistent_ttl(&env, &slash_count_key);
            }
        }

        // Finalise dispute
        dispute.resolved = true;
        dispute.ruling_for_depositor = ruling;
        env.storage().persistent().set(&dispute_key, &dispute);
        extend_persistent_ttl(&env, &dispute_key);

        // Settle escrow funds. Any milestone tranches already released via
        // `release_milestone_tranche` before the dispute was raised have
        // already left the contract, so only the amount still locked
        // (escrow.amount - cumulative released) is transferred here. When no
        // partial release has occurred `released` is 0 and this transfers
        // the full `escrow.amount`, matching prior behavior exactly.
        let escrow_key = DataKey::Escrow(escrow_id);
        let mut escrow: EscrowRecord = env
            .storage()
            .persistent()
            .get(&escrow_key)
            .ok_or(TrustFlowError::EscrowNotFound)?;
        let token: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token);

        let released: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::EscrowReleased(escrow_id))
            .unwrap_or(0);
        let remaining = escrow
            .amount
            .checked_sub(released)
            .ok_or(TrustFlowError::ArithmeticOverflow)?;

        let recipient = if ruling {
            escrow.depositor.clone()
        } else {
            escrow.beneficiary.clone()
        };
        if remaining > 0 {
            token_client.transfer(&env.current_contract_address(), &recipient, &remaining);
        }

        escrow.status = EscrowStatus::Settled;
        env.storage().persistent().set(&escrow_key, &escrow);
        extend_persistent_ttl(&env, &escrow_key);

        Ok(ruling)
    }

    // -----------------------------------------------------------------------
    // State archival / TTL maintenance
    // -----------------------------------------------------------------------

    /// Extend the TTL of an escrow's persistent storage (and its dispute
    /// records, if any) so it survives long periods of inactivity without
    /// being archived. Callable by anyone — e.g. a keeper/cron job — since it
    /// only pays for rent and cannot mutate escrow state or funds.
    ///
    /// Returns the ledger sequence up to which the escrow's storage is now
    /// guaranteed to live.
    pub fn bump_escrow_ttl(env: Env, escrow_id: u64) -> Result<u32, TrustFlowError> {
        let escrow_key = DataKey::Escrow(escrow_id);
        let escrow: EscrowRecord = env
            .storage()
            .persistent()
            .get(&escrow_key)
            .ok_or(TrustFlowError::EscrowNotFound)?;

        extend_instance_ttl(&env);
        extend_persistent_ttl(&env, &escrow_key);

        // Milestone-release accounting introduced alongside partial
        // settlement: the fee snapshot, the escrow-wide released counter,
        // and each milestone's own released counter all need to survive as
        // long as the escrow itself does.
        extend_persistent_ttl(&env, &DataKey::EscrowFeeConfig(escrow_id));
        extend_persistent_ttl(&env, &DataKey::EscrowReleased(escrow_id));
        for (index, _) in escrow.milestones.iter().enumerate() {
            extend_persistent_ttl(
                &env,
                &DataKey::MilestoneReleased(MilestoneKey {
                    escrow_id,
                    milestone_index: index as u32,
                }),
            );
        }

        let dispute_key = DataKey::Dispute(escrow_id);
        if env.storage().persistent().has(&dispute_key) {
            extend_persistent_ttl(&env, &dispute_key);
            let voters_key = DataKey::DisputeVoters(escrow_id);
            if env.storage().persistent().has(&voters_key) {
                extend_persistent_ttl(&env, &voters_key);
            }
        }

        let live_until_ledger = env
            .ledger()
            .sequence()
            .saturating_add(PERSISTENT_BUMP_AMOUNT);
        env.events().publish(
            (symbol_short!("escrow"), symbol_short!("ttlbump")),
            EscrowTtlBumped {
                escrow_id,
                live_until_ledger,
            },
        );

        Ok(live_until_ledger)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Events as _, Ledger as _},
        token, Address, Env, IntoVal, String, TryFromVal,
    };

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn create_token(env: &Env) -> (Address, token::StellarAssetClient<'_>) {
        let admin = Address::random(env);
        let contract = env.register_stellar_asset_contract(admin.clone());
        let sac = token::StellarAssetClient::new(env, &contract);
        (contract, sac)
    }

    fn setup(
        env: &Env,
        slash_bps: u32,
    ) -> (TrustFlowClient<'_>, Address, token::StellarAssetClient<'_>) {
        let (token_addr, sac) = create_token(env);
        let id = env.register_contract(None, TrustFlow);
        let client = TrustFlowClient::new(env, &id);
        let admin = Address::random(env);
        client.initialize(&admin, &token_addr, &slash_bps);
        (client, token_addr, sac)
    }

    fn mint(sac: &token::StellarAssetClient, to: &Address, amount: i128) {
        sac.mint(to, &amount);
    }

    fn balance(env: &Env, token_addr: &Address, addr: &Address) -> i128 {
        token::Client::new(env, token_addr).balance(addr)
    }

    // The four helpers below read contract storage directly (via
    // `env.as_contract`) rather than through public getters — the contract
    // intentionally exposes no `get_escrow`/`get_escrow_fee_config`/
    // `get_milestone_released`/`get_escrow_released` entrypoints, to keep
    // its public ABI (and on-chain spec) minimal.

    fn read_escrow(env: &Env, contract_id: &Address, escrow_id: u64) -> EscrowRecord {
        env.as_contract(contract_id, || {
            env.storage()
                .persistent()
                .get(&DataKey::Escrow(escrow_id))
                .unwrap()
        })
    }

    fn read_escrow_fee_config(env: &Env, contract_id: &Address, escrow_id: u64) -> EscrowFeeConfig {
        env.as_contract(contract_id, || {
            env.storage()
                .persistent()
                .get(&DataKey::EscrowFeeConfig(escrow_id))
                .unwrap()
        })
    }

    fn read_milestone_released(
        env: &Env,
        contract_id: &Address,
        escrow_id: u64,
        milestone_index: u32,
    ) -> i128 {
        env.as_contract(contract_id, || {
            env.storage()
                .persistent()
                .get(&DataKey::MilestoneReleased(MilestoneKey {
                    escrow_id,
                    milestone_index,
                }))
                .unwrap_or(0)
        })
    }

    fn read_escrow_released(env: &Env, contract_id: &Address, escrow_id: u64) -> i128 {
        env.as_contract(contract_id, || {
            env.storage()
                .persistent()
                .get(&DataKey::EscrowReleased(escrow_id))
                .unwrap_or(0)
        })
    }

    fn dispute_round(
        env: &Env,
        client: &TrustFlowClient,
        sac: &token::StellarAssetClient,
        _token_addr: &Address,
        honest_jurors: &[Address],
        malicious_juror: &Address,
        _slash_bps: u32,
    ) -> u64 {
        let depositor = Address::random(env);
        let beneficiary = Address::random(env);

        mint(sac, &depositor, 1_000);
        let escrow_id = client.create_escrow(&depositor, &beneficiary, &1_000);

        client.raise_dispute(
            &escrow_id,
            &depositor,
            &String::from_slice(env, "test dispute"),
        );

        // Majority (honest jurors) votes for depositor
        for j in honest_jurors {
            client.cast_vote(&escrow_id, j, &true);
        }
        // Malicious juror votes with minority (against depositor)
        client.cast_vote(&escrow_id, malicious_juror, &false);

        client.resolve_dispute(&escrow_id);
        escrow_id
    }

    // -----------------------------------------------------------------------
    // Basic staking
    // -----------------------------------------------------------------------

    #[test]
    fn test_stake_and_unstake() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let juror = Address::random(&env);
        mint(&sac, &juror, 500);

        client.stake(&juror, &500);
        assert_eq!(client.get_stake(&juror), 500);
        assert_eq!(balance(&env, &token_addr, &juror), 0);

        client.unstake(&juror, &200);
        assert_eq!(client.get_stake(&juror), 300);
        assert_eq!(balance(&env, &token_addr, &juror), 200);
    }

    #[test]
    fn test_unstake_insufficient_funds() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let juror = Address::random(&env);
        mint(&sac, &juror, 100);
        client.stake(&juror, &100);

        let result = client.try_unstake(&juror, &200);
        assert_eq!(result, Err(Ok(TrustFlowError::InsufficientStake)));
    }

    #[test]
    fn test_stake_zero_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, _sac) = setup(&env, DEFAULT_SLASH_BPS);
        let juror = Address::random(&env);

        let result = client.try_stake(&juror, &0);
        assert_eq!(result, Err(Ok(TrustFlowError::InvalidAmount)));
    }

    // -----------------------------------------------------------------------
    // Dispute voting
    // -----------------------------------------------------------------------

    #[test]
    fn test_cast_vote_requires_stake() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);
        let juror = Address::random(&env); // no stake

        mint(&sac, &depositor, 500);
        let escrow_id = client.create_escrow(&depositor, &beneficiary, &500);
        client.raise_dispute(&escrow_id, &depositor, &String::from_slice(&env, "test"));

        let result = client.try_cast_vote(&escrow_id, &juror, &true);
        assert_eq!(result, Err(Ok(TrustFlowError::InsufficientStake)));
    }

    #[test]
    fn test_cast_vote_duplicate_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);
        let juror = Address::random(&env);

        mint(&sac, &depositor, 500);
        mint(&sac, &juror, 200);
        client.stake(&juror, &200);

        let escrow_id = client.create_escrow(&depositor, &beneficiary, &500);
        client.raise_dispute(&escrow_id, &depositor, &String::from_slice(&env, "test"));
        client.cast_vote(&escrow_id, &juror, &true);

        let result = client.try_cast_vote(&escrow_id, &juror, &true);
        assert_eq!(result, Err(Ok(TrustFlowError::AlreadyVoted)));
    }

    // -----------------------------------------------------------------------
    // Single round slashing
    // -----------------------------------------------------------------------

    #[test]
    fn test_single_round_malicious_juror_slashed() {
        let env = Env::default();
        env.mock_all_auths();

        let slash_bps: u32 = 1_000; // 10%
        let (client, _token_addr, sac) = setup(&env, slash_bps);

        let honest1 = Address::random(&env);
        let honest2 = Address::random(&env);
        let malicious = Address::random(&env);

        mint(&sac, &honest1, 500);
        mint(&sac, &honest2, 500);
        mint(&sac, &malicious, 1_000);

        client.stake(&honest1, &500);
        client.stake(&honest2, &500);
        client.stake(&malicious, &1_000);

        let initial_stake = client.get_stake(&malicious);
        assert_eq!(initial_stake, 1_000);

        dispute_round(
            &env,
            &client,
            &sac,
            &_token_addr,
            &[honest1, honest2],
            &malicious,
            slash_bps,
        );

        let stake_after = client.get_stake(&malicious);
        // slashed 10% of 1_000 = 100
        assert_eq!(stake_after, 900);
        assert_eq!(client.get_slash_count(&malicious), 1);
    }

    #[test]
    fn test_honest_jurors_not_slashed() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, 1_000);

        let honest1 = Address::random(&env);
        let honest2 = Address::random(&env);
        let malicious = Address::random(&env);

        mint(&sac, &honest1, 500);
        mint(&sac, &honest2, 500);
        mint(&sac, &malicious, 500);

        client.stake(&honest1, &500);
        client.stake(&honest2, &500);
        client.stake(&malicious, &500);

        dispute_round(
            &env,
            &client,
            &sac,
            &_token_addr,
            &[honest1.clone(), honest2.clone()],
            &malicious,
            1_000,
        );

        // Honest jurors should be untouched
        assert_eq!(client.get_stake(&honest1), 500);
        assert_eq!(client.get_stake(&honest2), 500);
        assert_eq!(client.get_slash_count(&honest1), 0);
        assert_eq!(client.get_slash_count(&honest2), 0);
    }

    // -----------------------------------------------------------------------
    // Multiple dispute rounds – core acceptance criterion
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_rounds_malicious_juror_balance_decreases() {
        let env = Env::default();
        env.mock_all_auths();

        let slash_bps: u32 = 1_000; // 10% per round
        let (client, token_addr, sac) = setup(&env, slash_bps);

        let honest1 = Address::random(&env);
        let honest2 = Address::random(&env);
        let malicious = Address::random(&env);

        mint(&sac, &honest1, 10_000);
        mint(&sac, &honest2, 10_000);
        mint(&sac, &malicious, 10_000);

        client.stake(&honest1, &10_000);
        client.stake(&honest2, &10_000);
        client.stake(&malicious, &10_000);

        let initial_stake = client.get_stake(&malicious);

        // Round 1 – malicious juror votes against the majority
        dispute_round(
            &env,
            &client,
            &sac,
            &token_addr,
            &[honest1.clone(), honest2.clone()],
            &malicious,
            slash_bps,
        );
        let stake_r1 = client.get_stake(&malicious);
        assert!(
            stake_r1 < initial_stake,
            "stake must decrease after round 1"
        );
        assert_eq!(client.get_slash_count(&malicious), 1);

        // Round 2 – malicious juror repeats malicious behaviour
        dispute_round(
            &env,
            &client,
            &sac,
            &token_addr,
            &[honest1.clone(), honest2.clone()],
            &malicious,
            slash_bps,
        );
        let stake_r2 = client.get_stake(&malicious);
        assert!(
            stake_r2 < stake_r1,
            "stake must decrease further after round 2"
        );
        assert_eq!(client.get_slash_count(&malicious), 2);

        // Round 3 – a third consecutive malicious vote
        dispute_round(
            &env,
            &client,
            &sac,
            &token_addr,
            &[honest1.clone(), honest2.clone()],
            &malicious,
            slash_bps,
        );
        let stake_r3 = client.get_stake(&malicious);
        assert!(
            stake_r3 < stake_r2,
            "stake must decrease further after round 3"
        );
        assert_eq!(client.get_slash_count(&malicious), 3);

        // Verify the compounding slash: after 3 rounds of 10% each the
        // remaining stake approximates 10_000 * 0.9^3 = 7_290.
        // Due to integer truncation the result is >= 7_290 and < 7_300.
        assert!(
            stake_r3 >= 7_290 && stake_r3 <= 7_300,
            "expected ~7290 after 3×10% slashes, got {stake_r3}"
        );
    }

    #[test]
    fn test_four_rounds_progressive_slashing() {
        let env = Env::default();
        env.mock_all_auths();

        let slash_bps: u32 = 2_000; // 20% per round
        let (client, token_addr, sac) = setup(&env, slash_bps);

        let honest = Address::random(&env);
        let malicious = Address::random(&env);

        mint(&sac, &honest, 5_000);
        mint(&sac, &malicious, 10_000);

        // honest juror always forms the majority (needs only 1 honest > 0 malicious to win)
        // We use 2 honest voters to ensure a clear majority even with 1 malicious voter.
        let honest2 = Address::random(&env);
        mint(&sac, &honest2, 5_000);

        client.stake(&honest, &5_000);
        client.stake(&honest2, &5_000);
        client.stake(&malicious, &10_000);

        let mut prev_stake = client.get_stake(&malicious);

        for round in 1u32..=4 {
            dispute_round(
                &env,
                &client,
                &sac,
                &token_addr,
                &[honest.clone(), honest2.clone()],
                &malicious,
                slash_bps,
            );
            let current_stake = client.get_stake(&malicious);
            assert!(
                current_stake < prev_stake,
                "round {round}: stake must decrease, prev={prev_stake} current={current_stake}",
            );
            assert_eq!(client.get_slash_count(&malicious), round);
            prev_stake = current_stake;
        }
    }

    // -----------------------------------------------------------------------
    // Tie-breaking
    // -----------------------------------------------------------------------

    #[test]
    fn test_tie_favours_depositor() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, token_addr, sac) = setup(&env, 1_000);

        let juror_a = Address::random(&env); // votes for depositor
        let juror_b = Address::random(&env); // votes for beneficiary

        mint(&sac, &juror_a, 500);
        mint(&sac, &juror_b, 500);

        client.stake(&juror_a, &500);
        client.stake(&juror_b, &500);

        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);
        mint(&sac, &depositor, 1_000);

        let escrow_id = client.create_escrow(&depositor, &beneficiary, &1_000);
        client.raise_dispute(
            &escrow_id,
            &depositor,
            &String::from_slice(&env, "tie-break test"),
        );
        client.cast_vote(&escrow_id, &juror_a, &true); // for depositor
        client.cast_vote(&escrow_id, &juror_b, &false); // for beneficiary

        let ruling = client.resolve_dispute(&escrow_id);
        assert!(ruling, "tie should rule for depositor");

        // juror_b voted with minority and must be slashed
        assert_eq!(client.get_slash_count(&juror_b), 1);
        assert!(client.get_stake(&juror_b) < 500);

        // juror_a voted with majority and must be unaffected
        assert_eq!(client.get_slash_count(&juror_a), 0);
        assert_eq!(client.get_stake(&juror_a), 500);

        // depositor must have received the escrow funds back
        assert_eq!(balance(&env, &token_addr, &depositor), 1_000);
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_dispute_no_votes_fails() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, 1_000);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);

        mint(&sac, &depositor, 500);
        let escrow_id = client.create_escrow(&depositor, &beneficiary, &500);
        client.raise_dispute(&escrow_id, &depositor, &String::from_slice(&env, "test"));

        let result = client.try_resolve_dispute(&escrow_id);
        assert_eq!(result, Err(Ok(TrustFlowError::NoVotesCast)));
    }

    #[test]
    fn test_resolve_already_resolved_fails() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, 1_000);
        let juror = Address::random(&env);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);

        mint(&sac, &juror, 500);
        mint(&sac, &depositor, 500);

        client.stake(&juror, &500);
        let escrow_id = client.create_escrow(&depositor, &beneficiary, &500);
        client.raise_dispute(&escrow_id, &depositor, &String::from_slice(&env, "test"));
        client.cast_vote(&escrow_id, &juror, &true);
        client.resolve_dispute(&escrow_id);

        let result = client.try_resolve_dispute(&escrow_id);
        assert_eq!(result, Err(Ok(TrustFlowError::DisputeAlreadyResolved)));
    }

    #[test]
    fn test_slash_cannot_exceed_stake() {
        let env = Env::default();
        env.mock_all_auths();

        // Use 100% slash rate – stake should floor to 0, not go negative
        let (client, token_addr, sac) = setup(&env, 10_000);

        let honest = Address::random(&env);
        let honest2 = Address::random(&env);
        let malicious = Address::random(&env);

        mint(&sac, &honest, 500);
        mint(&sac, &honest2, 500);
        mint(&sac, &malicious, 300);

        client.stake(&honest, &500);
        client.stake(&honest2, &500);
        client.stake(&malicious, &300);

        dispute_round(
            &env,
            &client,
            &sac,
            &token_addr,
            &[honest, honest2],
            &malicious,
            10_000,
        );

        // 100% slash: stake should be zero, not negative
        assert_eq!(client.get_stake(&malicious), 0);
    }

    #[test]
    fn test_init_escrow_with_milestones() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);

        mint(&sac, &depositor, 1_000);

        let milestones = Vec::from_array(
            &env,
            [
                Milestone {
                    label: String::from_slice(&env, "Design"),
                    amount: 400,
                    approved: false,
                },
                Milestone {
                    label: String::from_slice(&env, "Development"),
                    amount: 600,
                    approved: false,
                },
            ],
        );

        let escrow_id = client.init_escrow(&depositor, &beneficiary, &milestones);

        // Verify balance transfer
        assert_eq!(balance(&env, &token_addr, &depositor), 0);
        assert_eq!(balance(&env, &token_addr, &client.address), 1_000);

        // Verify escrow ID increment
        assert_eq!(escrow_id, 1);
    }

    // -----------------------------------------------------------------------
    // State archival / TTL bump strategy
    // -----------------------------------------------------------------------

    /// Advance the ledger sequence number, simulating the passage of time.
    fn advance_ledger(env: &Env, ledgers: u32) {
        env.ledger().with_mut(|li| {
            li.sequence_number = li.sequence_number.saturating_add(ledgers);
        });
    }

    #[test]
    fn test_create_escrow_survives_default_ttl_window() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);
        mint(&sac, &depositor, 500);

        let escrow_id = client.create_escrow(&depositor, &beneficiary, &500);

        // The SDK's default persistent entry TTL is only 4096 ledgers
        // (~a few hours). Advance well beyond that but still comfortably
        // inside our 90-day bump window, and confirm the escrow is still
        // reachable — proving `create_escrow`'s immediate TTL bump is what
        // keeps it alive rather than the host's default.
        advance_ledger(&env, 20_000);

        client.raise_dispute(
            &escrow_id,
            &depositor,
            &String::from_slice(&env, "still here"),
        );
    }

    #[test]
    fn test_bump_escrow_ttl_keeps_dormant_escrow_alive() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);
        mint(&sac, &depositor, 500);

        let escrow_id = client.create_escrow(&depositor, &beneficiary, &500);

        // Escrow sits untouched for a long time (e.g. a long milestone
        // deadline). Advance to just inside the bump threshold window so a
        // keeper's call to the permissionless maintenance entrypoint
        // actually rewrites the TTL (the host no-ops bumps requested too
        // early), then advance further.
        advance_ledger(
            &env,
            PERSISTENT_BUMP_AMOUNT - PERSISTENT_LIFETIME_THRESHOLD + 1,
        );
        client.bump_escrow_ttl(&escrow_id);

        // Advance past what the escrow's *original* un-bumped TTL (granted
        // at creation) would have allowed — it only survives this because
        // of the keeper's bump above.
        advance_ledger(&env, PERSISTENT_LIFETIME_THRESHOLD);

        client.raise_dispute(
            &escrow_id,
            &depositor,
            &String::from_slice(&env, "kept alive by keeper"),
        );
    }

    #[test]
    fn test_bump_escrow_ttl_also_refreshes_dispute_storage() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let juror = Address::random(&env);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);
        mint(&sac, &juror, 500);
        mint(&sac, &depositor, 500);
        client.stake(&juror, &500);

        let escrow_id = client.create_escrow(&depositor, &beneficiary, &500);
        client.raise_dispute(
            &escrow_id,
            &depositor,
            &String::from_slice(&env, "slow juror"),
        );

        // A keeper sweep well before anything would lapse: bumps the escrow
        // (and its dispute) as well as the juror's still-idle stake, since
        // they haven't voted on anything yet either.
        advance_ledger(
            &env,
            PERSISTENT_BUMP_AMOUNT - PERSISTENT_LIFETIME_THRESHOLD + 1,
        );
        client.bump_escrow_ttl(&escrow_id);
        client.bump_juror_stake_ttl(&juror);
        advance_ledger(&env, PERSISTENT_LIFETIME_THRESHOLD);

        // The dispute record, its voter list, and the juror's stake must
        // all still be reachable well past the escrow's original TTL.
        client.cast_vote(&escrow_id, &juror, &true);
    }

    #[test]
    fn test_bump_escrow_ttl_nonexistent_escrow_fails() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, _sac) = setup(&env, DEFAULT_SLASH_BPS);

        let result = client.try_bump_escrow_ttl(&999);
        assert_eq!(result, Err(Ok(TrustFlowError::EscrowNotFound)));
    }

    #[test]
    fn test_bump_escrow_ttl_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);
        mint(&sac, &depositor, 500);

        let escrow_id = client.create_escrow(&depositor, &beneficiary, &500);
        // Push the entry to the edge of its bump window so the follow-up
        // call is guaranteed to perform (not skip) the TTL rewrite.
        advance_ledger(
            &env,
            PERSISTENT_BUMP_AMOUNT - PERSISTENT_LIFETIME_THRESHOLD + 1,
        );

        let live_until = client.bump_escrow_ttl(&escrow_id);

        let expected_live_until = env
            .ledger()
            .sequence()
            .saturating_add(PERSISTENT_BUMP_AMOUNT);
        assert_eq!(live_until, expected_live_until);

        let (event_contract, event_topics, event_data) = env.events().all().last().unwrap();
        assert_eq!(event_contract, client.address);

        let expected_topics: soroban_sdk::Vec<soroban_sdk::Val> =
            (symbol_short!("escrow"), symbol_short!("ttlbump")).into_val(&env);
        assert_eq!(event_topics, expected_topics);

        let decoded = EscrowTtlBumped::try_from_val(&env, &event_data).unwrap();
        assert_eq!(decoded.escrow_id, escrow_id);
        assert_eq!(decoded.live_until_ledger, expected_live_until);
    }

    #[test]
    fn test_bump_escrow_ttl_triggers_exactly_at_threshold_boundary() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);
        mint(&sac, &depositor, 500);

        let escrow_id = client.create_escrow(&depositor, &beneficiary, &500);

        // Advance so the escrow has *exactly* PERSISTENT_LIFETIME_THRESHOLD
        // ledgers of TTL remaining (created at seq 0 with an expiration of
        // PERSISTENT_BUMP_AMOUNT, so this advance leaves precisely that much
        // remaining) — the boundary the host's `remaining <= threshold`
        // check is defined on.
        advance_ledger(&env, PERSISTENT_BUMP_AMOUNT - PERSISTENT_LIFETIME_THRESHOLD);
        client.bump_escrow_ttl(&escrow_id);

        // Advance one ledger past what the *original* un-bumped expiration
        // would have been. If the threshold check were exclusive (`<`
        // instead of `<=`) the bump above would have been skipped as "not
        // yet due" and this read would hit a truly expired entry; since it
        // succeeds, the refresh fired at exactly the documented boundary,
        // not one ledger late.
        advance_ledger(&env, PERSISTENT_LIFETIME_THRESHOLD + 1);

        client.raise_dispute(
            &escrow_id,
            &depositor,
            &String::from_slice(&env, "right at the boundary"),
        );
    }

    #[test]
    fn test_bump_juror_stake_ttl_nonexistent_juror_fails() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, _sac) = setup(&env, DEFAULT_SLASH_BPS);
        let juror = Address::random(&env);

        let result = client.try_bump_juror_stake_ttl(&juror);
        assert_eq!(result, Err(Ok(TrustFlowError::JurorStakeNotFound)));
    }

    #[test]
    fn test_bump_juror_stake_ttl_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let juror = Address::random(&env);
        mint(&sac, &juror, 500);
        client.stake(&juror, &500);

        advance_ledger(
            &env,
            PERSISTENT_BUMP_AMOUNT - PERSISTENT_LIFETIME_THRESHOLD + 1,
        );

        let live_until = client.bump_juror_stake_ttl(&juror);

        let expected_live_until = env
            .ledger()
            .sequence()
            .saturating_add(PERSISTENT_BUMP_AMOUNT);
        assert_eq!(live_until, expected_live_until);

        let (event_contract, event_topics, event_data) = env.events().all().last().unwrap();
        assert_eq!(event_contract, client.address);

        let expected_topics: soroban_sdk::Vec<soroban_sdk::Val> =
            (symbol_short!("juror"), symbol_short!("ttlbump")).into_val(&env);
        assert_eq!(event_topics, expected_topics);

        let decoded = JurorStakeTtlBumped::try_from_val(&env, &event_data).unwrap();
        assert_eq!(decoded.juror, juror);
        assert_eq!(decoded.live_until_ledger, expected_live_until);
    }

    #[test]
    fn test_instance_ttl_survives_long_dormancy_between_calls() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);

        // `initialize` already bumped instance TTL; simulate the contract
        // going untouched for a long stretch and confirm it is still usable
        // (the instance, holding Admin/Token/SlashBps/EscrowCounter, was not
        // archived in the meantime).
        advance_ledger(&env, 20_000);

        let juror = Address::random(&env);
        mint(&sac, &juror, 100);
        client.stake(&juror, &100);
        assert_eq!(client.get_stake(&juror), 100);
    }

    // -----------------------------------------------------------------------
    // Atomic partial-milestone release with fee split to treasury
    // -----------------------------------------------------------------------

    /// Creates a single-milestone escrow of `milestone_amount` and returns
    /// `(depositor, beneficiary, escrow_id)`.
    fn setup_milestone_escrow(
        env: &Env,
        client: &TrustFlowClient,
        sac: &token::StellarAssetClient,
        milestone_amount: i128,
    ) -> (Address, Address, u64) {
        let depositor = Address::random(env);
        let beneficiary = Address::random(env);
        mint(sac, &depositor, milestone_amount);

        let milestones = Vec::from_array(
            env,
            [Milestone {
                label: String::from_slice(env, "Milestone 1"),
                amount: milestone_amount,
                approved: false,
            }],
        );
        let escrow_id = client.init_escrow(&depositor, &beneficiary, &milestones);
        (depositor, beneficiary, escrow_id)
    }

    #[test]
    fn test_release_milestone_exact_fee_split() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 10_000);
        let treasury = client.get_treasury();

        client.release_milestone_tranche(&escrow_id, &0u32, &10_000, &depositor);

        // Default fee is 50 bps (0.50%): floor(10_000 * 50 / 10_000) = 50.
        assert_eq!(balance(&env, &token_addr, &beneficiary), 9_950);
        assert_eq!(balance(&env, &token_addr, &treasury), 50);
        assert_eq!(balance(&env, &token_addr, &client.address), 0);

        let escrow = read_escrow(&env, &client.address, escrow_id);
        assert_eq!(escrow.status, EscrowStatus::Settled);
        assert!(escrow.milestones.get(0).unwrap().approved);
    }

    #[test]
    fn test_release_milestone_rounding_zero_fee_tranche() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);
        let treasury = client.get_treasury();

        // floor(100 * 50 / 10_000) = floor(0.5) = 0: the tranche fee rounds
        // down to zero and the full tranche goes to the beneficiary.
        client.release_milestone_tranche(&escrow_id, &0u32, &100, &depositor);

        assert_eq!(balance(&env, &token_addr, &beneficiary), 100);
        assert_eq!(balance(&env, &token_addr, &treasury), 0);
        assert_eq!(
            read_milestone_released(&env, &client.address, escrow_id, 0u32),
            100
        );
    }

    #[test]
    fn test_fragmented_releases_charge_same_total_fee_as_single_release() {
        let total_gross: i128 = 333;
        let expected_total_fee = (total_gross * DEFAULT_FEE_BPS as i128) / BPS_DENOMINATOR;

        // Fragmented scenario: the same total gross amount released across
        // three tranches, in its own Env so nothing else can contaminate the
        // treasury balance being measured.
        let env_fragmented = Env::default();
        env_fragmented.mock_all_auths();
        let (client_f, token_addr_f, sac_f) = setup(&env_fragmented, DEFAULT_SLASH_BPS);
        let treasury_f = client_f.get_treasury();
        let (depositor_f, beneficiary_f, escrow_f) =
            setup_milestone_escrow(&env_fragmented, &client_f, &sac_f, total_gross);

        let treasury_before_fragmented = balance(&env_fragmented, &token_addr_f, &treasury_f);
        let beneficiary_before_fragmented = balance(&env_fragmented, &token_addr_f, &beneficiary_f);
        client_f.release_milestone_tranche(&escrow_f, &0u32, &111, &depositor_f);
        client_f.release_milestone_tranche(&escrow_f, &0u32, &111, &depositor_f);
        client_f.release_milestone_tranche(&escrow_f, &0u32, &111, &depositor_f);
        let fragmented_total_fee =
            balance(&env_fragmented, &token_addr_f, &treasury_f) - treasury_before_fragmented;
        let fragmented_total_payout =
            balance(&env_fragmented, &token_addr_f, &beneficiary_f) - beneficiary_before_fragmented;

        // One-shot scenario: same total gross amount released in a single
        // call, in a fully independent Env so balances, escrow IDs, events,
        // and treasury state cannot contaminate the fragmented scenario
        // above.
        let env_one_shot = Env::default();
        env_one_shot.mock_all_auths();
        let (client_o, token_addr_o, sac_o) = setup(&env_one_shot, DEFAULT_SLASH_BPS);
        let treasury_o = client_o.get_treasury();
        let (depositor_o, _beneficiary_o, escrow_o) =
            setup_milestone_escrow(&env_one_shot, &client_o, &sac_o, total_gross);

        let treasury_before_one_shot = balance(&env_one_shot, &token_addr_o, &treasury_o);
        client_o.release_milestone_tranche(&escrow_o, &0u32, &total_gross, &depositor_o);
        let one_shot_total_fee =
            balance(&env_one_shot, &token_addr_o, &treasury_o) - treasury_before_one_shot;

        assert_eq!(fragmented_total_fee, one_shot_total_fee);
        assert_eq!(fragmented_total_fee, expected_total_fee);
        assert_eq!(one_shot_total_fee, expected_total_fee);
        assert_eq!(fragmented_total_payout + fragmented_total_fee, total_gross);
    }

    #[test]
    fn test_multiple_partial_releases_accumulate() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, _beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);

        client.release_milestone_tranche(&escrow_id, &0u32, &400, &depositor);
        assert_eq!(
            read_milestone_released(&env, &client.address, escrow_id, 0u32),
            400
        );
        assert_eq!(read_escrow_released(&env, &client.address, escrow_id), 400);
        assert_eq!(
            read_escrow(&env, &client.address, escrow_id).status,
            EscrowStatus::Active
        );

        client.release_milestone_tranche(&escrow_id, &0u32, &300, &depositor);
        assert_eq!(
            read_milestone_released(&env, &client.address, escrow_id, 0u32),
            700
        );
        assert_eq!(read_escrow_released(&env, &client.address, escrow_id), 700);
        assert_eq!(
            read_escrow(&env, &client.address, escrow_id).status,
            EscrowStatus::Active
        );
    }

    #[test]
    fn test_final_release_settles_escrow() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let depositor = Address::random(&env);
        let beneficiary = Address::random(&env);
        mint(&sac, &depositor, 1_000);

        let milestones = Vec::from_array(
            &env,
            [
                Milestone {
                    label: String::from_slice(&env, "M1"),
                    amount: 400,
                    approved: false,
                },
                Milestone {
                    label: String::from_slice(&env, "M2"),
                    amount: 600,
                    approved: false,
                },
            ],
        );
        let escrow_id = client.init_escrow(&depositor, &beneficiary, &milestones);

        client.release_milestone_tranche(&escrow_id, &0u32, &400, &depositor);
        assert_eq!(
            read_escrow(&env, &client.address, escrow_id).status,
            EscrowStatus::Active
        );

        client.release_milestone_tranche(&escrow_id, &1u32, &600, &depositor);
        let escrow = read_escrow(&env, &client.address, escrow_id);
        assert_eq!(escrow.status, EscrowStatus::Settled);
        assert!(escrow.milestones.get(0).unwrap().approved);
        assert!(escrow.milestones.get(1).unwrap().approved);
    }

    #[test]
    fn test_release_zero_amount_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, _beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);

        let result = client.try_release_milestone_tranche(&escrow_id, &0u32, &0, &depositor);
        assert_eq!(result, Err(Ok(TrustFlowError::InvalidAmount)));
    }

    #[test]
    fn test_release_excessive_amount_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, _beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);

        let result = client.try_release_milestone_tranche(&escrow_id, &0u32, &1_001, &depositor);
        assert_eq!(result, Err(Ok(TrustFlowError::MilestoneAmountMismatch)));

        // A valid partial release followed by an over-release of the
        // remainder must also fail, and must not mutate any state.
        client.release_milestone_tranche(&escrow_id, &0u32, &600, &depositor);
        let result = client.try_release_milestone_tranche(&escrow_id, &0u32, &500, &depositor);
        assert_eq!(result, Err(Ok(TrustFlowError::MilestoneAmountMismatch)));
        assert_eq!(
            read_milestone_released(&env, &client.address, escrow_id, 0u32),
            600
        );
    }

    #[test]
    fn test_release_nonexistent_milestone_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, _beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);

        let result = client.try_release_milestone_tranche(&escrow_id, &5u32, &100, &depositor);
        assert_eq!(result, Err(Ok(TrustFlowError::MilestoneNotFound)));
    }

    #[test]
    fn test_release_unauthorized_caller_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (_depositor, beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);

        // The beneficiary is not the depositor and must not be able to
        // authorize a release.
        let result = client.try_release_milestone_tranche(&escrow_id, &0u32, &500, &beneficiary);
        assert_eq!(result, Err(Ok(TrustFlowError::Unauthorized)));
    }

    #[test]
    fn test_release_on_disputed_escrow_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, _beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);

        client.raise_dispute(&escrow_id, &depositor, &String::from_slice(&env, "test"));

        let result = client.try_release_milestone_tranche(&escrow_id, &0u32, &500, &depositor);
        assert_eq!(result, Err(Ok(TrustFlowError::InvalidState)));
    }

    #[test]
    fn test_release_on_settled_escrow_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, _beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);

        client.release_milestone_tranche(&escrow_id, &0u32, &1_000, &depositor);
        assert_eq!(
            read_escrow(&env, &client.address, escrow_id).status,
            EscrowStatus::Settled
        );

        let result = client.try_release_milestone_tranche(&escrow_id, &0u32, &1, &depositor);
        assert_eq!(result, Err(Ok(TrustFlowError::InvalidState)));
    }

    #[test]
    fn test_set_fee_bps_authorization_and_limits() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, _sac) = setup(&env, DEFAULT_SLASH_BPS);
        // Default treasury is the admin, so this is the real stored admin.
        let admin = client.get_treasury();

        assert_eq!(client.get_fee_bps(), DEFAULT_FEE_BPS);

        client.set_fee_bps(&admin, &200);
        assert_eq!(client.get_fee_bps(), 200);

        // Exceeds the 1_000 bps cap.
        let result = client.try_set_fee_bps(&admin, &1_001);
        assert_eq!(result, Err(Ok(TrustFlowError::InvalidFeeBps)));
        assert_eq!(client.get_fee_bps(), 200);

        // A non-admin caller must be rejected even though `mock_all_auths`
        // makes every `require_auth()` call succeed.
        let stranger = Address::random(&env);
        let result = client.try_set_fee_bps(&stranger, &100);
        assert_eq!(result, Err(Ok(TrustFlowError::Unauthorized)));
        assert_eq!(client.get_fee_bps(), 200);
    }

    #[test]
    fn test_set_treasury_authorization() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, _sac) = setup(&env, DEFAULT_SLASH_BPS);
        let admin = client.get_treasury();
        let new_treasury = Address::random(&env);

        // A non-admin caller must be rejected even though `mock_all_auths`
        // makes every `require_auth()` call succeed.
        let stranger = Address::random(&env);
        let result = client.try_set_treasury(&stranger, &new_treasury);
        assert_eq!(result, Err(Ok(TrustFlowError::Unauthorized)));
        assert_eq!(client.get_treasury(), admin);

        client.set_treasury(&admin, &new_treasury);
        assert_eq!(client.get_treasury(), new_treasury);
    }

    #[test]
    fn test_escrow_fee_snapshot_immutable_after_fee_change() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let admin = client.get_treasury();

        let (_depositor, _beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);
        let snapshot_before = read_escrow_fee_config(&env, &client.address, escrow_id);
        assert_eq!(snapshot_before.fee_bps, DEFAULT_FEE_BPS);

        client.set_fee_bps(&admin, &500);

        let snapshot_after = read_escrow_fee_config(&env, &client.address, escrow_id);
        assert_eq!(snapshot_after, snapshot_before);
        assert_eq!(snapshot_after.fee_bps, DEFAULT_FEE_BPS);

        // A new escrow created after the change picks up the new default.
        let depositor2 = Address::random(&env);
        let beneficiary2 = Address::random(&env);
        mint(&sac, &depositor2, 1_000);
        let escrow_id_2 = client.create_escrow(&depositor2, &beneficiary2, &1_000);
        let snapshot_2 = read_escrow_fee_config(&env, &client.address, escrow_id_2);
        assert_eq!(snapshot_2.fee_bps, 500);
    }

    #[test]
    fn test_escrow_treasury_snapshot_immutable_after_treasury_change() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let admin = client.get_treasury();

        let (_depositor, _beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);
        let snapshot_before = read_escrow_fee_config(&env, &client.address, escrow_id);
        assert_eq!(snapshot_before.treasury, admin);

        let new_treasury = Address::random(&env);
        client.set_treasury(&admin, &new_treasury);

        let snapshot_after = read_escrow_fee_config(&env, &client.address, escrow_id);
        assert_eq!(snapshot_after, snapshot_before);
        assert_eq!(snapshot_after.treasury, admin);

        // A new escrow created after the change picks up the new treasury.
        let depositor2 = Address::random(&env);
        let beneficiary2 = Address::random(&env);
        mint(&sac, &depositor2, 1_000);
        let escrow_id_2 = client.create_escrow(&depositor2, &beneficiary2, &1_000);
        let snapshot_2 = read_escrow_fee_config(&env, &client.address, escrow_id_2);
        assert_eq!(snapshot_2.treasury, new_treasury);
    }

    #[test]
    fn test_release_milestone_tranche_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 10_000);
        let treasury = client.get_treasury();

        client.release_milestone_tranche(&escrow_id, &0u32, &10_000, &depositor);

        let (event_contract, event_topics, event_data) = env.events().all().last().unwrap();
        assert_eq!(event_contract, client.address);

        let expected_topics: soroban_sdk::Vec<soroban_sdk::Val> = (
            symbol_short!("mstone"),
            symbol_short!("release"),
            escrow_id,
            0u32,
        )
            .into_val(&env);
        assert_eq!(event_topics, expected_topics);

        let decoded = MilestoneTrancheReleased::try_from_val(&env, &event_data).unwrap();
        assert_eq!(decoded.gross_amount, 10_000);
        assert_eq!(decoded.treasury_fee, 50);
        assert_eq!(decoded.beneficiary_payout, 9_950);
        assert_eq!(decoded.milestone_released, 10_000);
        assert_eq!(decoded.escrow_released, 10_000);
        assert_eq!(decoded.beneficiary, beneficiary);
        assert_eq!(decoded.treasury, treasury);
    }

    #[test]
    fn test_raise_dispute_refreshes_partial_release_accounting_ttl() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, _beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);

        client.release_milestone_tranche(&escrow_id, &0u32, &400, &depositor);

        advance_ledger(
            &env,
            PERSISTENT_BUMP_AMOUNT - PERSISTENT_LIFETIME_THRESHOLD + 1,
        );

        client.raise_dispute(
            &escrow_id,
            &depositor,
            &String::from_slice(&env, "ttl regression"),
        );

        // Move beyond the original expiration of the release counter.
        // No token invocation is needed after this point.
        advance_ledger(&env, PERSISTENT_LIFETIME_THRESHOLD);

        assert_eq!(read_escrow_released(&env, &client.address, escrow_id), 400);
        assert_eq!(
            read_escrow(&env, &client.address, escrow_id).status,
            EscrowStatus::Disputed
        );
    }
    #[test]
    fn test_dispute_settlement_after_partial_release_transfers_remaining_only() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 1_000);

        // Partial release before the dispute is raised.
        client.release_milestone_tranche(&escrow_id, &0u32, &400, &depositor);
        let beneficiary_balance_after_partial = balance(&env, &token_addr, &beneficiary);

        client.raise_dispute(
            &escrow_id,
            &depositor,
            &String::from_slice(&env, "scope changed"),
        );

        let juror = Address::random(&env);
        mint(&sac, &juror, 500);
        client.stake(&juror, &500);
        client.cast_vote(&escrow_id, &juror, &false); // rules for beneficiary

        let contract_balance_before = balance(&env, &token_addr, &client.address);
        client.resolve_dispute(&escrow_id);

        // Only the still-locked 600 (1_000 - 400 already released) may move
        // — transferring the original full 1_000 here would double-pay the
        // beneficiary for the 400 already released.
        assert_eq!(
            balance(&env, &token_addr, &beneficiary),
            beneficiary_balance_after_partial + 600
        );
        assert_eq!(
            balance(&env, &token_addr, &client.address),
            contract_balance_before - 600
        );

        let escrow = read_escrow(&env, &client.address, escrow_id);
        assert_eq!(escrow.status, EscrowStatus::Settled);
    }

    #[test]
    fn test_no_balance_leakage_matches_gross_release() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, _beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 10_000);

        let contract_balance_before = balance(&env, &token_addr, &client.address);

        client.release_milestone_tranche(&escrow_id, &0u32, &3_333, &depositor);
        client.release_milestone_tranche(&escrow_id, &0u32, &3_333, &depositor);
        client.release_milestone_tranche(&escrow_id, &0u32, &3_334, &depositor);

        let contract_balance_after = balance(&env, &token_addr, &client.address);
        // The contract's balance must drop by exactly the gross amount
        // released — no more (leaked) and no less (stuck) than what left as
        // beneficiary payout + treasury fee.
        assert_eq!(contract_balance_before - contract_balance_after, 10_000);

        let escrow = read_escrow(&env, &client.address, escrow_id);
        assert_eq!(escrow.status, EscrowStatus::Settled);
    }

    // -----------------------------------------------------------------------
    // Atomic rollback on transfer failure
    // -----------------------------------------------------------------------

    #[test]
    fn test_release_milestone_tranche_rolls_back_atomically_on_transfer_failure() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, token_addr, sac) = setup(&env, DEFAULT_SLASH_BPS);
        let (depositor, beneficiary, escrow_id) =
            setup_milestone_escrow(&env, &client, &sac, 10_000);
        let treasury = client.get_treasury();

        let token_client = token::Client::new(&env, &token_addr);

        // Default fee is 50 bps: gross 10_000 splits into beneficiary
        // payout 9_950 and treasury fee 50. Burn exactly 50 from the
        // TrustFlow contract so the first transfer can execute but the
        // second transfer fails due to insufficient remaining balance.
        token_client.burn(&client.address, &50);

        let beneficiary_balance_before = balance(&env, &token_addr, &beneficiary);
        let treasury_balance_before = balance(&env, &token_addr, &treasury);
        let contract_balance_before = balance(&env, &token_addr, &client.address);
        assert_eq!(contract_balance_before, 9_950);
        assert_eq!(beneficiary_balance_before, 0);
        assert_eq!(treasury_balance_before, 0);

        let escrow_before = read_escrow(&env, &client.address, escrow_id);
        assert_eq!(escrow_before.amount, 10_000);

        let milestone_released_before =
            read_milestone_released(&env, &client.address, escrow_id, 0u32);
        let escrow_released_before = read_escrow_released(&env, &client.address, escrow_id);

        let result = client.try_release_milestone_tranche(&escrow_id, &0u32, &10_000, &depositor);
        assert_eq!(result, Err(Ok(TrustFlowError::TokenTransferFailed)));

        // Nothing must have moved or been recorded: the failed treasury leg
        // must roll back the beneficiary transfer and all accounting too.
        // The pre-existing burn is not itself rolled back -- it happened
        // before (and independently of) the failed release invocation.
        assert_eq!(
            balance(&env, &token_addr, &beneficiary),
            beneficiary_balance_before
        );
        assert_eq!(
            balance(&env, &token_addr, &treasury),
            treasury_balance_before
        );
        assert_eq!(balance(&env, &token_addr, &client.address), 9_950);

        let escrow_after = read_escrow(&env, &client.address, escrow_id);
        assert_eq!(escrow_after.status, EscrowStatus::Active);
        assert_eq!(escrow_after.status, escrow_before.status);
        assert!(!escrow_after.milestones.get(0).unwrap().approved);
        assert_eq!(
            read_milestone_released(&env, &client.address, escrow_id, 0u32),
            milestone_released_before
        );
        assert_eq!(
            read_escrow_released(&env, &client.address, escrow_id),
            escrow_released_before
        );

        // Failed token calls may remain visible in the test event log.
        // Verify specifically that TrustFlow did not emit its success event.
        let expected_topics: soroban_sdk::Vec<soroban_sdk::Val> = (
            symbol_short!("mstone"),
            symbol_short!("release"),
            escrow_id,
            0u32,
        )
            .into_val(&env);

        let emitted_release_event =
            env.events()
                .all()
                .iter()
                .any(|(event_contract, event_topics, _)| {
                    event_contract == client.address && event_topics == expected_topics
                });

        assert!(!emitted_release_event);
    }
}
