# GASLESS canonical ATA policy

Upstream baseline: `8c592591debd08424a65cc471ce0403578fd5d5d`

This fork preserves global `allow_create_account = false`. Its Swap payer-funded account-creation exception is one direct (`stackHeight = 2`) System `CreateAccount` CPI beneath an outer Associated Token Program `CreateIdempotent` instruction. That exception also requires exactly one outer Jupiter v6 instruction and at least one direct (`stackHeight = 2`) Raydium CLMM CPI beneath that Jupiter instruction; an outer Raydium instruction or a missing/mis-parented CPI is rejected.

The standalone SEND policy is a separate policy branch. Runtime configuration supplies a treasury wallet and a list of exact legacy SPL mint/decimals pairs; it never supplies a recipient. Kora derives the mint, recipient, requested amount, canonical source, canonical recipient ATA, and canonical treasury ATA from the exact user-authorized transaction and current RPC state. It permits optional Compute Budget instructions followed by either three positive `TransferChecked` instructions for an existing recipient ATA, or one canonical recipient ATA `CreateIdempotent` plus those same three transfers when the ATA is absent. The first transfer is the exact recipient transfer; reimbursement and service fee both use the same mint and target ATA(treasury, mint, legacy Token Program). Source and treasury accounts must already be healthy, and an existing recipient ATA must be healthy and canonical. Swap programs, Token-2022, inner transfers, invalid or off-curve recipients, wrong rent/space/owner, additional signers, and additional payer-funded System actions are denied.

The fork receives no browser-controlled policy metadata. It derives the payer, user signer, output mint, destination ATA, owner, space, and lamports from the exact transaction and verifies the destination's pre-execution absence and current 165-byte rent minimum through the configured Solana RPC. Eligible output mints are server configuration. The exception fails closed when simulation provenance is absent or ambiguous.

Required configuration:

```toml
[validation.fee_payer_policy.system]
allow_create_account = false

[validation.fee_payer_policy.system.canonical_ata_creation]
enabled = true
allowed_output_mints = ["<exact legacy SPL output mint>"]

[validation.fee_payer_policy.system.send]
enabled = true
settlement_wallet = "EEFxZ3mtdPXNKkBQkbuAw1HBPvQvU2HVKWvRVbuciSsb"
approved_mints = [
  { mint = "<exact legacy SPL SEND mint>", decimals = 6 },
]
```

Adding a SEND token is a validated runtime configuration change, not a Kora recompile: approve the exact mint in GASLESS, create its canonical treasury ATA, add the mint to Kora `allowed_tokens` and `send.approved_mints`, then pass Kora RPC config validation. Startup/config RPC validation fails if an approved SEND mint is not a healthy initialized legacy SPL mint with the configured decimals, or if ATA(treasury, mint, legacy Token Program) is absent or unhealthy.

All existing authentication, method, signer, program allowlist, payer-outflow, fee, and transaction validation remains in force. `CreateAccountWithSeed`, direct CreateAccount, payer transfers, allocate, assign, multiple creations, wrong parent/depth, non-canonical destinations, non-legacy token ownership, wrong rent/space, and non-Jupiter/Raydium swap shapes remain denied.

## Local-only CLEAN policy

This policy is disabled by default and has not been deployed:

```toml
[validation.fee_payer_policy.system.clean]
claim_enabled = false
burn_enabled = false
settlement_wallet = "EEFxZ3mtdPXNKkBQkbuAw1HBPvQvU2HVKWvRVbuciSsb"
fee_bps = 300
maximum_claim_accounts = 10
```

Claim/Burn validation is shape-specific: v0, exactly payer and user signers, exact 375000/100000 compute prefix, current ordinary legacy SPL state, user close destination, exact full-balance Burn where applicable, and exact 3% rent fee plus message network fee. Global create-account, System transfer, SPL burn, and SPL close permissions must remain false. Recover uses the separate narrow policy below.

## Recover Value policy

Recover is disabled by default. When explicitly configured, Kora decodes the Jupiter v6
`SharedAccountsRoute` instruction and derives its effective minimum output from the instruction's
quoted output and the required 50-bps slippage. The exact route accounts, Raydium CLMM pool and
direct CPI provenance, full authoritative source balance, canonical wSOL lifecycle, compute
budget, settlement, fees, reimbursement, payer, and user remain independently bound.

The configuration does not contain a spot-price snapshot. `catastrophe_output_lamports` is a
deliberately broad secondary bound, set to the product's 1,000,000-lamport minimum-outcome scale,
that rejects effectively worthless swap output without tracking normal market movement.
`minimum_user_payout_lamports` independently requires the transaction's guaranteed output plus
recovered rent, less the exact service fees and sponsored network cost, to preserve the configured
minimum user outcome. GASLESS remains authoritative for fresh quote TTL, exact input/output mints,
50-bps slippage, the 100-bps price-impact ceiling, and exact-message binding. A changed economic
message requires a new server quote and user signature.
