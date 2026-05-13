# ⛓️ TrustFlow Contract

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/Rust-1.74.0-orange)](https://www.rust-lang.org/)
[![Soroban SDK](https://img.shields.io/badge/Soroban_SDK-20.0.0-blue)](https://soroban.stellar.org/)

> **Battle-tested Soroban smart contracts powering TrustFlow's decentralized escrow protocol.**

TrustFlow Contract is the on-chain layer of the TrustFlow Protocol — a suite of Rust smart contracts deployed on the Stellar/Soroban network. It handles milestone-based escrow, community dispute resolution, fee management, oracle integration, and on-chain governance for the gig economy.

---

## ✨ Core Features

- 🔒 **Milestone Escrow**: Trustless vault contracts that release funds tranche-by-tranche as work is approved.
- ⚖️ **Dispute Resolution**: Decentralized courtroom with juror voting, settlement logic, and appeals.
- 🏛️ **On-Chain Governance**: Protocol parameter changes governed by token holders.
- 🔮 **Oracle Integration**: External price feeds and data anchoring for real-world settlement.
- 💸 **Fee Engine**: Configurable fee collection and distribution across protocol participants.
- 🪙 **Abundance Token**: Native fungible token contract for protocol incentives.
- 🌱 **Crowdfund Contract**: Milestone-based fundraising with built-in accountability.

---

## 🗂️ Project Structure

```
contracts/
├── src/                    # Core protocol contracts
│   ├── dispute.rs          # Dispute creation, voting, and resolution
│   ├── settlement.rs       # Fund release and settlement logic
│   ├── governance.rs       # On-chain governance and voting
│   ├── oracle.rs           # External data feed integration
│   ├── fee.rs              # Fee collection and distribution
│   ├── storage.rs          # Persistent contract storage definitions
│   ├── types.rs            # Shared data types and structs
│   ├── events.rs           # Contract event definitions
│   └── errors.rs           # Error codes and handling
├── abundance/              # Abundance token (SAC-compatible fungible token)
├── crowdfund/              # Crowdfund contract
scripts/
├── deploy.sh               # Deploy contracts to network
├── migrate.sh              # Run contract migrations
├── seed.sh                 # Seed test data
└── setup.sh                # Environment setup
tests/
├── escrow.test.ts          # Escrow contract tests
├── dispute.test.ts         # Dispute resolution tests
├── auth.test.ts            # Auth and access control tests
├── stellar.test.ts         # Stellar network integration tests
└── integration.test.ts     # Full end-to-end integration tests
```

---

## 🚀 Getting Started

### Prerequisites

- Rust >= 1.74.0
- Soroban CLI
- Stellar account funded on testnet

### Install Rust & Soroban CLI

```bash
rustup target add wasm32-unknown-unknown
cargo install --locked soroban-cli
```

### Build Contracts

```bash
make build
# or
cargo build --target wasm32-unknown-unknown --release
```

### Deploy to Testnet

```bash
./scripts/setup.sh
./scripts/deploy.sh testnet
```

### Run Tests

```bash
cargo test
```

---

## 📖 Contract Reference

### Escrow / Dispute Flow

1. Client creates an escrow vault via the SDK or backend API.
2. Funds are locked into the contract.
3. Milestones are released as work is approved.
4. If disputed, jurors vote on-chain — settlement is executed automatically.

### Governance

Protocol parameters (fees, quorum thresholds, juror rewards) are controlled by on-chain proposals and token-weighted voting.

### Oracle

Price feeds are anchored via the oracle contract, enabling USDC-denominated payouts at fair market rates.

---

## 🛡️ Security

- Overflow checks enabled in all release builds.
- LTO and symbol stripping applied for minimal attack surface.
- All contract errors are explicitly typed via `errors.rs`.
- Events emitted for every state transition — fully auditable on-chain.

---

## 🗺️ Roadmap

- [ ] **Multi-sig Escrow**: Corporate escrow requiring M-of-N approvals.
- [ ] **Reputation Oracle**: On-chain reputation scores anchored to work history.
- [ ] **DAO Treasury**: Protocol fee accumulation and community-governed spending.
- [ ] **Formal Verification**: TLA+ spec for core escrow state machine.

---

## 🤝 Community & Support

- **Documentation**: [Full Protocol Docs](https://docs.trustflow.xyz)
- **Issues**: [Report bugs or request features](https://github.com/trustflow-protocol/trustflow-contract/issues)
- **Discussions**: [Stellar Community Forum](https://stellar.org/community)

---

*Securing the future of work, one transaction at a time.*

---

## 📜 License

MIT License. Copyright (c) 2026 TrustFlow Protocol.
