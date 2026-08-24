# GASLESS canonical ATA policy

Upstream baseline: `8c592591debd08424a65cc471ce0403578fd5d5d`

This fork preserves global `allow_create_account = false`. Its only payer-funded account-creation exception is one direct (`stackHeight = 2`) System `CreateAccount` CPI beneath an outer Associated Token Program `CreateIdempotent` instruction. The exception also requires exactly one outer Jupiter v6 instruction and at least one direct (`stackHeight = 2`) Raydium CLMM CPI beneath that Jupiter instruction; an outer Raydium instruction or a missing/mis-parented CPI is rejected.

The fork receives no browser-controlled policy metadata. It derives the payer, user signer, output mint, destination ATA, owner, space, and lamports from the exact transaction and verifies the destination's pre-execution absence and current 165-byte rent minimum through the configured Solana RPC. Eligible output mints are server configuration. The exception fails closed when simulation provenance is absent or ambiguous.

Required configuration:

```toml
[validation.fee_payer_policy.system]
allow_create_account = false

[validation.fee_payer_policy.system.canonical_ata_creation]
enabled = true
allowed_output_mints = ["<exact legacy SPL output mint>"]
```

All existing authentication, method, signer, program allowlist, payer-outflow, fee, and transaction validation remains in force. `CreateAccountWithSeed`, direct CreateAccount, transfers, allocate, assign, multiple creations, wrong parent/depth, non-canonical destinations, non-legacy token ownership, wrong rent/space, existing destinations, and non-Jupiter/Raydium swap shapes remain denied.
