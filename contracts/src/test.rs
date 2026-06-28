#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Events as _},
    Address, Env, String,
};

use crate::errors::TrustFlowError;
use crate::storage::DataKey;
use crate::types::{DisputeRecord, EscrowRecord, EscrowStatus};
use crate::{TrustFlowContract, TrustFlowContractClient};

struct Setup {
    env: Env,
    contract_id: Address,
    depositor: Address,
    beneficiary: Address,
    outsider: Address,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, TrustFlowContract);
    Setup {
        depositor: Address::random(&env),
        beneficiary: Address::random(&env),
        outsider: Address::random(&env),
        contract_id,
        env,
    }
}

fn seed_escrow(s: &Setup, id: u64, status: EscrowStatus) {
    let token = Address::random(&s.env);
    let depositor = s.depositor.clone();
    let beneficiary = s.beneficiary.clone();
    s.env.as_contract(&s.contract_id, || {
        let rec = EscrowRecord {
            id,
            depositor,
            beneficiary,
            amount: 1_000,
            token,
            status,
            created_at: 0,
            release_deadline: 0,
        };
        s.env.storage().persistent().set(&DataKey::Escrow(id), &rec);
    });
}

fn escrow_status(s: &Setup, id: u64) -> EscrowStatus {
    s.env.as_contract(&s.contract_id, || {
        s.env
            .storage()
            .persistent()
            .get::<DataKey, EscrowRecord>(&DataKey::Escrow(id))
            .unwrap()
            .status
    })
}

fn dispute_record(s: &Setup, id: u64) -> Option<DisputeRecord> {
    s.env.as_contract(&s.contract_id, || {
        s.env
            .storage()
            .persistent()
            .get::<DataKey, DisputeRecord>(&DataKey::Dispute(id))
    })
}

#[test]
fn depositor_can_raise_dispute() {
    let s = setup();
    seed_escrow(&s, 1, EscrowStatus::Active);
    let client = TrustFlowContractClient::new(&s.env, &s.contract_id);

    client.raise_dispute(
        &1,
        &s.depositor,
        &String::from_slice(&s.env, "not delivered"),
    );

    assert_eq!(escrow_status(&s, 1), EscrowStatus::Disputed);
    let d = dispute_record(&s, 1).unwrap();
    assert_eq!(d.raised_by, s.depositor);
    assert!(!d.resolved);
    // exactly one event published (the dispute notification for indexers)
    assert_eq!(s.env.events().all().len(), 1);
}

#[test]
fn beneficiary_can_raise_dispute() {
    let s = setup();
    seed_escrow(&s, 1, EscrowStatus::Active);
    let client = TrustFlowContractClient::new(&s.env, &s.contract_id);

    client.raise_dispute(
        &1,
        &s.beneficiary,
        &String::from_slice(&s.env, "wrong amount"),
    );

    assert_eq!(escrow_status(&s, 1), EscrowStatus::Disputed);
    assert_eq!(dispute_record(&s, 1).unwrap().raised_by, s.beneficiary);
}

#[test]
fn outsider_cannot_raise_dispute() {
    let s = setup();
    seed_escrow(&s, 1, EscrowStatus::Active);
    let client = TrustFlowContractClient::new(&s.env, &s.contract_id);

    assert_eq!(
        client.try_raise_dispute(&1, &s.outsider, &String::from_slice(&s.env, "meddling")),
        Err(Ok(TrustFlowError::Unauthorized))
    );
    // escrow is untouched and no dispute was recorded
    assert_eq!(escrow_status(&s, 1), EscrowStatus::Active);
    assert!(dispute_record(&s, 1).is_none());
}

#[test]
fn cannot_raise_on_missing_escrow() {
    let s = setup();
    let client = TrustFlowContractClient::new(&s.env, &s.contract_id);

    assert_eq!(
        client.try_raise_dispute(&99, &s.depositor, &String::from_slice(&s.env, "ghost")),
        Err(Ok(TrustFlowError::EscrowNotFound))
    );
}

#[test]
fn cannot_raise_on_non_active_escrow() {
    let s = setup();
    seed_escrow(&s, 1, EscrowStatus::Released);
    let client = TrustFlowContractClient::new(&s.env, &s.contract_id);

    assert_eq!(
        client.try_raise_dispute(&1, &s.depositor, &String::from_slice(&s.env, "too late")),
        Err(Ok(TrustFlowError::InvalidState))
    );
}

#[test]
fn cannot_raise_twice() {
    let s = setup();
    seed_escrow(&s, 1, EscrowStatus::Active);
    let client = TrustFlowContractClient::new(&s.env, &s.contract_id);

    client.raise_dispute(&1, &s.depositor, &String::from_slice(&s.env, "first"));
    // escrow is now Disputed, so a second raise is rejected as InvalidState
    assert_eq!(
        client.try_raise_dispute(&1, &s.beneficiary, &String::from_slice(&s.env, "second")),
        Err(Ok(TrustFlowError::InvalidState))
    );
}
