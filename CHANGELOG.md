# Changelog

## Commit-Reveal Juror Voting

### New: commit-reveal voting for dispute resolution

Replaces the previous direct `cast_vote` flow with a two-phase
commit-reveal pattern to eliminate vote-copying and bribery vectors.

**How it works:**

1. **Commit phase** (`commit_vote`) — jurors submit
   `sha256(vote_byte ++ salt)` before the commit window closes. No vote
   direction is revealed.
2. **Reveal phase** (`reveal_vote`) — after the commit window closes,
   jurors disclose their `vote_for_depositor` flag and 32-byte `salt`.
   The contract verifies the hash matches the stored commitment.
3. **Resolution** (`resolve_dispute`) — after the reveal window closes,
   the majority rules (ties favour the depositor); minority voters are
   slashed.

**Hash preimage format** (for wallet/keeper integrators):

```
sha256( [vote_byte, salt[0], salt[1], ..., salt[31]] )
```

- `vote_byte`: `0x01` (for depositor) or `0x00` (for beneficiary)
- `salt`: exactly 32 bytes of randomness (enforced by `BytesN<32>`)

**Key behaviours:**

- Commitments are keyed by `(escrow_id, juror)` — the same salt can be
  reused across different disputes without collision.
- A juror who commits but never reveals is **not** penalised; their vote
  is simply not counted.
- After a successful reveal, the stored commitment is cleared to free
  storage and prevent accidental reuse.
- Commit and reveal windows default to 17,280 ledgers (~1 day each).
  These are currently constants; a follow-up PR may make them
  admin-configurable.

**Migration / integrator notes:**

- Wallets and keepers must implement a two-step UX: commit first, then
  reveal after the commit window closes.
- The salt must be exactly 32 bytes. Store it alongside the commitment
  off-chain; it cannot be recovered if lost.
