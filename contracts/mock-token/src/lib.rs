#![no_std]

//! Mock Token Contract for Testing
//! 
//! A simple Stellar Asset Contract (SAC) compliant token implementation
//! designed for testing escrow and payment flows with USDC-like behavior.

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, String};

#[contracttype]
pub enum DataKey {
    Balance(Address),
    Admin,
    Metadata,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenMetadata {
    pub decimal: u32,
    pub name: String,
    pub symbol: String,
}

#[contract]
pub struct MockToken;

#[contractimpl]
impl MockToken {
    /// Initialize the mock token with metadata
    pub fn initialize(e: Env, admin: Address, decimal: u32, name: String, symbol: String) {
        if e.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }

        e.storage().instance().set(&DataKey::Admin, &admin);
        e.storage().instance().set(
            &DataKey::Metadata,
            &TokenMetadata {
                decimal,
                name,
                symbol,
            },
        );
    }

    /// Mint tokens to an address (testing only)
    pub fn mint(e: Env, to: Address, amount: i128) {
        let admin: Address = e.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();

        if amount < 0 {
            panic!("amount cannot be negative");
        }

        let balance = Self::balance(e.clone(), to.clone());
        e.storage()
            .persistent()
            .set(&DataKey::Balance(to), &(balance + amount));
    }

    /// Transfer tokens from one address to another
    pub fn transfer(e: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();

        if amount < 0 {
            panic!("amount cannot be negative");
        }

        let from_balance = Self::balance(e.clone(), from.clone());
        if from_balance < amount {
            panic!("insufficient balance");
        }

        e.storage()
            .persistent()
            .set(&DataKey::Balance(from.clone()), &(from_balance - amount));

        let to_balance = Self::balance(e.clone(), to.clone());
        e.storage()
            .persistent()
            .set(&DataKey::Balance(to), &(to_balance + amount));
    }

    /// Get balance of an address
    pub fn balance(e: Env, addr: Address) -> i128 {
        e.storage()
            .persistent()
            .get(&DataKey::Balance(addr))
            .unwrap_or(0)
    }

    /// Get token decimals
    pub fn decimals(e: Env) -> u32 {
        let metadata: TokenMetadata = e.storage().instance().get(&DataKey::Metadata).unwrap();
        metadata.decimal
    }

    /// Get token name
    pub fn name(e: Env) -> String {
        let metadata: TokenMetadata = e.storage().instance().get(&DataKey::Metadata).unwrap();
        metadata.name
    }

    /// Get token symbol
    pub fn symbol(e: Env) -> String {
        let metadata: TokenMetadata = e.storage().instance().get(&DataKey::Metadata).unwrap();
        metadata.symbol
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Address, Env};

    #[test]
    fn test_initialize() {
        let env = Env::default();
        let contract_id = env.register_contract(None, MockToken);
        let client = MockTokenClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(
            &admin,
            &6,
            &String::from_str(&env, "Mock USDC"),
            &String::from_str(&env, "MUSDC"),
        );

        assert_eq!(client.decimals(), 6);
        assert_eq!(client.name(), String::from_str(&env, "Mock USDC"));
        assert_eq!(client.symbol(), String::from_str(&env, "MUSDC"));
    }

    #[test]
    fn test_mint_and_balance() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, MockToken);
        let client = MockTokenClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        client.initialize(
            &admin,
            &6,
            &String::from_str(&env, "Mock USDC"),
            &String::from_str(&env, "MUSDC"),
        );

        assert_eq!(client.balance(&user), 0);

        client.mint(&user, &1000);
        assert_eq!(client.balance(&user), 1000);
    }

    #[test]
    fn test_transfer() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, MockToken);
        let client = MockTokenClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let user1 = Address::generate(&env);
        let user2 = Address::generate(&env);

        client.initialize(
            &admin,
            &6,
            &String::from_str(&env, "Mock USDC"),
            &String::from_str(&env, "MUSDC"),
        );

        client.mint(&user1, &1000);
        assert_eq!(client.balance(&user1), 1000);
        assert_eq!(client.balance(&user2), 0);

        client.transfer(&user1, &user2, &400);
        assert_eq!(client.balance(&user1), 600);
        assert_eq!(client.balance(&user2), 400);
    }

    #[test]
    #[should_panic(expected = "insufficient balance")]
    fn test_transfer_insufficient_balance() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, MockToken);
        let client = MockTokenClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let user1 = Address::generate(&env);
        let user2 = Address::generate(&env);

        client.initialize(
            &admin,
            &6,
            &String::from_str(&env, "Mock USDC"),
            &String::from_str(&env, "MUSDC"),
        );

        client.mint(&user1, &100);
        client.transfer(&user1, &user2, &200);
    }
}
