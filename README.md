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

### State Archival & TTL Bump Strategy

Soroban archives `persistent` and `instance` storage entries once their time-to-live (TTL) expires; an archived entry can only be read again after a separate `RestoreFootprintOp`. Because TrustFlow escrows and disputes are often long-lived (milestone projects, jurors slow to vote), the `trustflow` contract (`contracts/trustflow/src/lib.rs`) manages this explicitly instead of relying on the SDK's short default TTL:

- Every persistent write (escrow, dispute, votes, juror stake) is immediately followed by a TTL bump to a 90-day horizon, refreshed once the entry drops under 30 days remaining.
- The contract **instance** (admin/token/config) is bumped on the same 90-day/30-day schedule on every call. This is intentional: the instance must outlive any single escrow's dormancy window, since a call is needed just to invoke anything — including the maintenance calls below — and an archived instance bricks the whole contract, not just one escrow.
- `bump_escrow_ttl(escrow_id)` and `bump_juror_stake_ttl(juror)` are permissionless maintenance entrypoints a keeper/cron job can call periodically to keep a fully dormant escrow or an idle juror's stake alive without mutating any state. Both emit events (`EscrowTtlBumped`) for indexers to track rent health.

---

### Atomic Partial-Milestone Release with Fee Split to Treasury

`release_milestone_tranche(escrow_id, milestone_index, gross_amount, caller)` (in `contracts/trustflow/src/lib.rs`) lets a depositor release a milestone's funded amount in one or more partial tranches, splitting a protocol fee to the treasury atomically in the same call:

- **Default fee & treasury**: `initialize` sets a default protocol fee of **50 bps (0.50%)**, paid to the **admin** address, without changing the existing `initialize(admin, token, slash_bps)` signature. The admin can raise or lower the global default fee (up to a **1,000 bps / 10% cap**) via `set_fee_bps`, and repoint the global default treasury via `set_treasury` — both require the stored admin's authorization. `get_fee_bps`/`get_treasury` expose the current global defaults; there are no getters for per-escrow or per-milestone accounting, which is internal contract state.
- **Per-escrow fee snapshot**: `create_escrow` and `init_escrow` both snapshot the current global fee bps and treasury address onto the escrow at creation time. Later `set_fee_bps`/`set_treasury` calls only affect escrows created afterward — an escrow's fee terms never change mid-flight.
- **Cumulative fee-delta rounding**: each tranche's fee is charged as `cumulative_fee(released_after) - cumulative_fee(released_before)`, where `cumulative_fee(x) = floor(x * fee_bps / 10_000)` is computed with checked, overflow-safe quotient/remainder arithmetic. This guarantees splitting one release into many tranches never changes the total fee collected, and every tranche satisfies `gross_amount = beneficiary_payout + treasury_fee` exactly.
- **Atomic two-recipient settlement**: cumulative-release accounting is validated and persisted *before* any token transfer; the beneficiary payout and treasury fee are then both transferred in the same invocation, and a zero-valued fee is simply skipped. If either transfer fails, Soroban rolls back the entire call, so books can never end up mismatched. A milestone's `approved` flag is set on every authorized release, and the escrow is marked `Settled` only once its full funded amount has been released. `resolve_dispute` transfers only the amount still locked, using checked subtraction (unaffected when no partial release occurred).
- **Event**: each release emits `MilestoneTrancheReleased`, with `escrow_id`/`milestone_index` carried as indexed topics (not duplicated in the event data) so indexers can filter without decoding every event.
- **No new dependencies**: the feature is implemented entirely with the existing `soroban-sdk` primitives already used elsewhere in the contract (persistent storage, checked `i128` arithmetic, events) — no new crates were added.

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
