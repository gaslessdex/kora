use crate::{
    config::FeePayerPolicy,
    error::KoraError,
    fee::fee::{FeeConfigUtil, TotalFeeCalculation},
    oracle::PriceSource,
    state::get_config,
    token::{interface::TokenMint, token::TokenUtil},
    transaction::{
        ParsedSPLInstructionData, ParsedSPLInstructionType, ParsedSystemInstructionData,
        ParsedSystemInstructionType, VersionedTransactionResolved,
    },
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    account::Account, instruction::Instruction, pubkey::Pubkey, transaction::VersionedTransaction,
};
use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_PROGRAM_ID};
use std::{collections::HashSet, str::FromStr};

use crate::fee::price::PriceModel;

const JUPITER_V6_PROGRAM_ID: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
const RAYDIUM_CLMM_PROGRAM_ID: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
const METEORA_DLMM_PROGRAM_ID: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const PUMPSWAP_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const PUMP_FEE_PROGRAM_ID: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
const PUMP_GLOBAL_CONFIG: &str = "ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw";
const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
// This validator intentionally supports Raydium's legacy SPL-token-only `swap`
// account layout. Its 13-account shape is different from `swap_v2`, even though
// both instructions encode the same four swap arguments after the discriminator.
const RAYDIUM_SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
const RAYDIUM_SWAP_V2_DISCRIMINATOR: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];
const RAYDIUM_POOL_DISCRIMINATOR: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70];
const RAYDIUM_AMM_CONFIG_DISCRIMINATOR: [u8; 8] = [218, 244, 33, 104, 203, 203, 43, 111];
const RAYDIUM_OBSERVATION_DISCRIMINATOR: [u8; 8] = [122, 174, 197, 53, 129, 9, 165, 132];
const RAYDIUM_TICK_ARRAY_DISCRIMINATOR: [u8; 8] = [192, 155, 85, 205, 49, 249, 129, 42];
const RAYDIUM_BITMAP_DISCRIMINATOR: [u8; 8] = [60, 150, 36, 219, 97, 128, 139, 153];
const METEORA_SWAP2_DISCRIMINATOR: [u8; 8] = [65, 75, 63, 76, 235, 91, 91, 136];
const METEORA_SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
const METEORA_LB_PAIR_DISCRIMINATOR: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13];
const METEORA_BIN_ARRAY_DISCRIMINATOR: [u8; 8] = [92, 142, 92, 220, 5, 148, 70, 181];
const METEORA_BITMAP_DISCRIMINATOR: [u8; 8] = [80, 111, 124, 113, 55, 237, 18, 5];
const METEORA_ORACLE_DISCRIMINATOR: [u8; 8] = [139, 194, 131, 179, 140, 179, 229, 244];
const PUMPSWAP_BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
const PUMPSWAP_BUY_EXACT_QUOTE_DISCRIMINATOR: [u8; 8] = [198, 46, 21, 82, 180, 217, 232, 112];
const PUMPSWAP_SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
const PUMPSWAP_POOL_DISCRIMINATOR: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
const PUMPSWAP_GLOBAL_DISCRIMINATOR: [u8; 8] = [149, 8, 156, 202, 160, 252, 176, 217];
const PUMPSWAP_GLOBAL_VOLUME_DISCRIMINATOR: [u8; 8] = [202, 42, 246, 43, 142, 190, 30, 255];
const PUMPSWAP_USER_VOLUME_DISCRIMINATOR: [u8; 8] = [86, 255, 112, 14, 102, 53, 154, 250];
const SEND_COMPUTE_UNIT_LIMIT: u32 = 200_000;
const SEND_COMPUTE_UNIT_PRICE_MICROLAMPORTS: u64 = 375_000;

fn token_account_mint(
    account: &Account,
    token_program: Pubkey,
    authority: Pubkey,
) -> Option<Pubkey> {
    if account.owner != token_program
        || account.data.len() != 165
        || account.data[32..64] != authority.to_bytes()
        || account.data[108] != 1
        || account.data[72..76] != [0, 0, 0, 0]
        || account.data[121..129] != [0; 8]
        || account.data[129..133] != [0, 0, 0, 0]
    {
        return None;
    }
    let mint = Pubkey::new_from_array(account.data[..32].try_into().ok()?);
    let native_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").ok()?;
    let native_tag = &account.data[109..113];
    if (mint == native_mint
        && (native_tag != [1, 0, 0, 0]
            || u64::from_le_bytes(account.data[113..121].try_into().ok()?) > account.lamports))
        || (mint != native_mint && native_tag != [0, 0, 0, 0])
    {
        return None;
    }
    Some(mint)
}

fn valid_legacy_token_account(
    account: &Account,
    token_program: Pubkey,
    mint: Pubkey,
    authority: Pubkey,
) -> bool {
    token_account_mint(account, token_program, authority) == Some(mint)
}

fn legacy_mint_decimals(account: &Account, token_program: Pubkey) -> Option<u8> {
    if account.owner != token_program || account.data.len() != 82 || account.data[45] != 1 {
        return None;
    }
    Some(account.data[44])
}

pub struct TransactionValidator {
    fee_payer_pubkey: Pubkey,
    max_allowed_lamports: u64,
    allowed_programs: Vec<Pubkey>,
    max_signatures: u64,
    allowed_tokens: Vec<Pubkey>,
    disallowed_accounts: Vec<Pubkey>,
    _price_source: PriceSource,
    fee_payer_policy: FeePayerPolicy,
}

impl TransactionValidator {
    pub fn new(fee_payer_pubkey: Pubkey) -> Result<Self, KoraError> {
        let config = &get_config()?.validation;

        // Convert string program IDs to Pubkeys
        let allowed_programs = config
            .allowed_programs
            .iter()
            .map(|addr| {
                Pubkey::from_str(addr).map_err(|e| {
                    KoraError::InternalServerError(format!(
                        "Invalid program address in config: {e}"
                    ))
                })
            })
            .collect::<Result<Vec<Pubkey>, KoraError>>()?;

        Ok(Self {
            fee_payer_pubkey,
            max_allowed_lamports: config.max_allowed_lamports,
            allowed_programs,
            max_signatures: config.max_signatures,
            _price_source: config.price_source.clone(),
            allowed_tokens: config
                .allowed_tokens
                .iter()
                .map(|addr| Pubkey::from_str(addr))
                .collect::<Result<Vec<Pubkey>, _>>()
                .map_err(|e| {
                    KoraError::InternalServerError(format!("Invalid allowed token address: {e}"))
                })?,
            disallowed_accounts: config
                .disallowed_accounts
                .iter()
                .map(|addr| Pubkey::from_str(addr))
                .collect::<Result<Vec<Pubkey>, _>>()
                .map_err(|e| {
                    KoraError::InternalServerError(format!(
                        "Invalid disallowed account address: {e}"
                    ))
                })?,
            fee_payer_policy: config.fee_payer_policy.clone(),
        })
    }

    pub async fn fetch_and_validate_token_mint(
        &self,
        mint: &Pubkey,
        rpc_client: &RpcClient,
    ) -> Result<Box<dyn TokenMint + Send + Sync>, KoraError> {
        // First check if the mint is in allowed tokens
        if !self.allowed_tokens.contains(mint) {
            return Err(KoraError::InvalidTransaction(format!(
                "Mint {mint} is not a valid token mint"
            )));
        }

        let mint = TokenUtil::get_mint(rpc_client, mint).await?;

        Ok(mint)
    }

    /*
    This function is used to validate a transaction.
     */
    pub async fn validate_transaction(
        &self,
        transaction_resolved: &mut VersionedTransactionResolved,
        rpc_client: &RpcClient,
    ) -> Result<(), KoraError> {
        if transaction_resolved.all_instructions.is_empty() {
            return Err(KoraError::InvalidTransaction(
                "Transaction contains no instructions".to_string(),
            ));
        }

        if transaction_resolved.all_account_keys.is_empty() {
            return Err(KoraError::InvalidTransaction(
                "Transaction contains no account keys".to_string(),
            ));
        }

        self.validate_signatures(&transaction_resolved.transaction)?;

        self.validate_programs(transaction_resolved)?;
        self.validate_transfer_amounts(transaction_resolved, rpc_client).await?;
        self.validate_disallowed_accounts(transaction_resolved)?;
        self.validate_fee_payer_usage(transaction_resolved, rpc_client).await?;

        Ok(())
    }

    pub fn validate_lamport_fee(&self, fee: u64) -> Result<(), KoraError> {
        if fee > self.max_allowed_lamports {
            return Err(KoraError::InvalidTransaction(format!(
                "Fee {} exceeds maximum allowed {}",
                fee, self.max_allowed_lamports
            )));
        }
        Ok(())
    }

    fn validate_signatures(&self, transaction: &VersionedTransaction) -> Result<(), KoraError> {
        if transaction.signatures.len() > self.max_signatures as usize {
            return Err(KoraError::InvalidTransaction(format!(
                "Too many signatures: {} > {}",
                transaction.signatures.len(),
                self.max_signatures
            )));
        }

        if transaction.signatures.is_empty() {
            return Err(KoraError::InvalidTransaction("No signatures found".to_string()));
        }

        Ok(())
    }

    fn validate_programs(
        &self,
        transaction_resolved: &VersionedTransactionResolved,
    ) -> Result<(), KoraError> {
        for instruction in &transaction_resolved.all_instructions {
            if !self.allowed_programs.contains(&instruction.program_id) {
                return Err(KoraError::InvalidTransaction(format!(
                    "Program {} is not in the allowed list",
                    instruction.program_id
                )));
            }
        }
        Ok(())
    }

    async fn validate_fee_payer_usage(
        &self,
        transaction_resolved: &mut VersionedTransactionResolved,
        rpc_client: &RpcClient,
    ) -> Result<(), KoraError> {
        let system_instructions = transaction_resolved.get_or_parse_system_instructions()?.clone();

        // Validate system program instructions
        validate_system!(self, &system_instructions, SystemTransfer,
            ParsedSystemInstructionData::SystemTransfer { sender, .. } => sender,
            self.fee_payer_policy.system.allow_transfer, "System Transfer");

        validate_system!(self, &system_instructions, SystemAssign,
            ParsedSystemInstructionData::SystemAssign { authority } => authority,
            self.fee_payer_policy.system.allow_assign, "System Assign");

        validate_system!(self, &system_instructions, SystemAllocate,
            ParsedSystemInstructionData::SystemAllocate { account } => account,
            self.fee_payer_policy.system.allow_allocate, "System Allocate");

        let payer_creations = system_instructions
            .get(&ParsedSystemInstructionType::SystemCreateAccount)
            .into_iter()
            .flatten()
            .filter(|instruction| {
                matches!(instruction,
                ParsedSystemInstructionData::SystemCreateAccount { payer, .. }
                    if *payer == self.fee_payer_pubkey)
            })
            .count();
        let jupiter_program =
            Pubkey::from_str(JUPITER_V6_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let outer_instruction_count = transaction_resolved.transaction.message.instructions().len();
        let outer = &transaction_resolved.all_instructions[..outer_instruction_count];
        let has_outer_jupiter =
            outer.iter().any(|instruction| instruction.program_id == jupiter_program);
        let has_clean_shape = !has_outer_jupiter
            && outer.iter().any(|instruction| {
                instruction.program_id == spl_token_interface::id()
                    && matches!(instruction.data.first(), Some(9 | 15))
            });
        let recover_close_count = outer
            .iter()
            .filter(|instruction| {
                instruction.program_id == spl_token_interface::id()
                    && matches!(instruction.data.first(), Some(9))
            })
            .count();
        let has_recover_shape = has_outer_jupiter && recover_close_count == 2;
        if has_recover_shape {
            self.validate_recover(transaction_resolved, rpc_client, payer_creations).await?;
        } else if has_outer_jupiter {
            self.validate_swap_route(transaction_resolved, rpc_client).await?;
            if payer_creations > 0 {
                self.validate_canonical_ata_creation(
                    transaction_resolved,
                    rpc_client,
                    payer_creations,
                )
                .await?;
            }
        } else if payer_creations > 0 && !self.fee_payer_policy.system.allow_create_account {
            self.validate_send(transaction_resolved, rpc_client, payer_creations).await?;
        } else if has_clean_shape
            && (self.fee_payer_policy.system.clean.claim_enabled
                || self.fee_payer_policy.system.clean.burn_enabled)
        {
            self.validate_clean(transaction_resolved, rpc_client).await?;
        } else if self.fee_payer_policy.system.send.enabled
            && !has_outer_jupiter
            && outer.iter().any(|instruction| instruction.program_id == spl_token_interface::id())
        {
            self.validate_send(transaction_resolved, rpc_client, payer_creations).await?;
        }

        validate_system!(self, &system_instructions, SystemInitializeNonceAccount,
            ParsedSystemInstructionData::SystemInitializeNonceAccount { nonce_authority, .. } => nonce_authority,
            self.fee_payer_policy.system.nonce.allow_initialize, "System Initialize Nonce Account");

        validate_system!(self, &system_instructions, SystemAdvanceNonceAccount,
            ParsedSystemInstructionData::SystemAdvanceNonceAccount { nonce_authority, .. } => nonce_authority,
            self.fee_payer_policy.system.nonce.allow_advance, "System Advance Nonce Account");

        validate_system!(self, &system_instructions, SystemAuthorizeNonceAccount,
            ParsedSystemInstructionData::SystemAuthorizeNonceAccount { nonce_authority, .. } => nonce_authority,
            self.fee_payer_policy.system.nonce.allow_authorize, "System Authorize Nonce Account");

        // Note: SystemUpgradeNonceAccount not validated - no authority parameter

        validate_system!(self, &system_instructions, SystemWithdrawNonceAccount,
            ParsedSystemInstructionData::SystemWithdrawNonceAccount { nonce_authority, .. } => nonce_authority,
            self.fee_payer_policy.system.nonce.allow_withdraw, "System Withdraw Nonce Account");

        // Validate SPL instructions
        let spl_instructions = transaction_resolved.get_or_parse_spl_instructions()?;

        validate_spl!(self, spl_instructions, SplTokenTransfer,
            ParsedSPLInstructionData::SplTokenTransfer { owner, is_2022, .. } => { owner, is_2022 },
            self.fee_payer_policy.spl_token.allow_transfer,
            self.fee_payer_policy.token_2022.allow_transfer,
            "SPL Token Transfer", "Token2022 Token Transfer");

        validate_spl!(self, spl_instructions, SplTokenApprove,
            ParsedSPLInstructionData::SplTokenApprove { owner, is_2022, .. } => { owner, is_2022 },
            self.fee_payer_policy.spl_token.allow_approve,
            self.fee_payer_policy.token_2022.allow_approve,
            "SPL Token Approve", "Token2022 Token Approve");

        validate_spl!(self, spl_instructions, SplTokenBurn,
            ParsedSPLInstructionData::SplTokenBurn { owner, is_2022 } => { owner, is_2022 },
            self.fee_payer_policy.spl_token.allow_burn,
            self.fee_payer_policy.token_2022.allow_burn,
            "SPL Token Burn", "Token2022 Token Burn");

        validate_spl!(self, spl_instructions, SplTokenCloseAccount,
            ParsedSPLInstructionData::SplTokenCloseAccount { owner, is_2022 } => { owner, is_2022 },
            self.fee_payer_policy.spl_token.allow_close_account,
            self.fee_payer_policy.token_2022.allow_close_account,
            "SPL Token Close Account", "Token2022 Token Close Account");

        validate_spl!(self, spl_instructions, SplTokenRevoke,
            ParsedSPLInstructionData::SplTokenRevoke { owner, is_2022 } => { owner, is_2022 },
            self.fee_payer_policy.spl_token.allow_revoke,
            self.fee_payer_policy.token_2022.allow_revoke,
            "SPL Token Revoke", "Token2022 Token Revoke");

        validate_spl!(self, spl_instructions, SplTokenSetAuthority,
            ParsedSPLInstructionData::SplTokenSetAuthority { authority, is_2022 } => { authority, is_2022 },
            self.fee_payer_policy.spl_token.allow_set_authority,
            self.fee_payer_policy.token_2022.allow_set_authority,
            "SPL Token SetAuthority", "Token2022 Token SetAuthority");

        validate_spl!(self, spl_instructions, SplTokenMintTo,
            ParsedSPLInstructionData::SplTokenMintTo { mint_authority, is_2022 } => { mint_authority, is_2022 },
            self.fee_payer_policy.spl_token.allow_mint_to,
            self.fee_payer_policy.token_2022.allow_mint_to,
            "SPL Token MintTo", "Token2022 Token MintTo");

        validate_spl!(self, spl_instructions, SplTokenInitializeMint,
            ParsedSPLInstructionData::SplTokenInitializeMint { mint_authority, is_2022 } => { mint_authority, is_2022 },
            self.fee_payer_policy.spl_token.allow_initialize_mint,
            self.fee_payer_policy.token_2022.allow_initialize_mint,
            "SPL Token InitializeMint", "Token2022 Token InitializeMint");

        validate_spl!(self, spl_instructions, SplTokenInitializeAccount,
            ParsedSPLInstructionData::SplTokenInitializeAccount { owner, is_2022 } => { owner, is_2022 },
            self.fee_payer_policy.spl_token.allow_initialize_account,
            self.fee_payer_policy.token_2022.allow_initialize_account,
            "SPL Token InitializeAccount", "Token2022 Token InitializeAccount");

        validate_spl_multisig!(self, spl_instructions, SplTokenInitializeMultisig,
            ParsedSPLInstructionData::SplTokenInitializeMultisig { signers, is_2022 } => { signers, is_2022 },
            self.fee_payer_policy.spl_token.allow_initialize_multisig,
            self.fee_payer_policy.token_2022.allow_initialize_multisig,
            "SPL Token InitializeMultisig", "Token2022 Token InitializeMultisig");

        validate_spl!(self, spl_instructions, SplTokenFreezeAccount,
            ParsedSPLInstructionData::SplTokenFreezeAccount { freeze_authority, is_2022 } => { freeze_authority, is_2022 },
            self.fee_payer_policy.spl_token.allow_freeze_account,
            self.fee_payer_policy.token_2022.allow_freeze_account,
            "SPL Token FreezeAccount", "Token2022 Token FreezeAccount");

        validate_spl!(self, spl_instructions, SplTokenThawAccount,
            ParsedSPLInstructionData::SplTokenThawAccount { freeze_authority, is_2022 } => { freeze_authority, is_2022 },
            self.fee_payer_policy.spl_token.allow_thaw_account,
            self.fee_payer_policy.token_2022.allow_thaw_account,
            "SPL Token ThawAccount", "Token2022 Token ThawAccount");

        Ok(())
    }

    async fn validate_clean(
        &self,
        transaction: &VersionedTransactionResolved,
        rpc_client: &RpcClient,
    ) -> Result<(), KoraError> {
        let policy = &self.fee_payer_policy.system.clean;
        let token_program = spl_token_interface::id();
        let compute_program = solana_compute_budget_interface::id();
        let settlement_wallet =
            Pubkey::from_str(&policy.settlement_wallet).map_err(|_| KoraError::ConfigError)?;
        let message = match &transaction.transaction.message {
            solana_message::VersionedMessage::V0(message)
                if message.address_table_lookups.is_empty() =>
            {
                message
            }
            _ => {
                return Err(KoraError::InvalidTransaction(
                    "CLEAN Claim/Burn requires a v0 message without lookup tables".to_string(),
                ))
            }
        };
        let signer_keys = transaction.transaction.message.static_account_keys();
        if transaction.transaction.message.header().num_required_signatures != 2
            || transaction.transaction.message.header().num_readonly_signed_accounts != 0
            || signer_keys.first() != Some(&self.fee_payer_pubkey)
            || signer_keys.get(1).is_none()
        {
            return Err(KoraError::InvalidTransaction(
                "CLEAN requires exactly the configured payer and user signers".to_string(),
            ));
        }
        let wallet = signer_keys[1];
        if wallet == self.fee_payer_pubkey || wallet == settlement_wallet {
            return Err(KoraError::InvalidTransaction(
                "CLEAN identities must be distinct".to_string(),
            ));
        }
        let outer_count = transaction.transaction.message.instructions().len();
        let outer = &transaction.all_instructions[..outer_count];
        if outer.len() < 4
            || outer[0].program_id != compute_program
            || outer[0].data.as_slice() != [3, 216, 184, 5, 0, 0, 0, 0, 0]
            || outer[1].program_id != compute_program
            || outer[1].data.len() != 5
            || outer[1].data[0] != 2
            || outer[2..].iter().any(|instruction| instruction.program_id == compute_program)
        {
            return Err(KoraError::InvalidTransaction(
                "CLEAN compute-budget prefix is invalid".to_string(),
            ));
        }
        let compute_limit =
            u32::from_le_bytes(outer[1].data[1..5].try_into().map_err(|_| KoraError::ConfigError)?);
        let transfer = outer.last().ok_or_else(|| {
            KoraError::InvalidTransaction("CLEAN settlement is missing".to_string())
        })?;
        let settlement_lamports = match bincode::deserialize::<SystemInstruction>(&transfer.data) {
            Ok(SystemInstruction::Transfer { lamports }) => lamports,
            _ => {
                return Err(KoraError::InvalidTransaction(
                    "CLEAN settlement must be one System transfer".to_string(),
                ))
            }
        };
        if transfer.program_id != SYSTEM_PROGRAM_ID
            || transfer.accounts.len() != 2
            || transfer.accounts[0].pubkey != wallet
            || transfer.accounts[1].pubkey != settlement_wallet
        {
            return Err(KoraError::InvalidTransaction(
                "CLEAN settlement accounts are invalid".to_string(),
            ));
        }
        let token_instructions = &outer[2..outer.len() - 1];
        let is_burn = token_instructions.first().and_then(|instruction| instruction.data.first())
            == Some(&15);
        if is_burn {
            if !policy.burn_enabled || compute_limit != 100_000 || token_instructions.len() != 2 {
                return Err(KoraError::InvalidTransaction(
                    "CLEAN Burn shape is disabled or invalid".to_string(),
                ));
            }
        } else if !policy.claim_enabled
            || compute_limit != 100_000
            || token_instructions.is_empty()
            || token_instructions.len() > usize::from(policy.maximum_claim_accounts)
        {
            return Err(KoraError::InvalidTransaction(
                "CLEAN Claim shape is disabled or invalid".to_string(),
            ));
        }
        let close_start = usize::from(is_burn);
        if token_instructions[close_start..].iter().any(|instruction| {
            instruction.program_id != token_program
                || !matches!(
                    spl_token_interface::instruction::TokenInstruction::unpack(&instruction.data),
                    Ok(spl_token_interface::instruction::TokenInstruction::CloseAccount)
                )
                || instruction.accounts.len() != 3
                || instruction.accounts[1].pubkey != wallet
                || instruction.accounts[2].pubkey != wallet
        }) {
            return Err(KoraError::InvalidTransaction(
                "CLEAN close-account fields are invalid".to_string(),
            ));
        }
        let source_keys = token_instructions[close_start..]
            .iter()
            .map(|instruction| instruction.accounts[0].pubkey)
            .collect::<Vec<_>>();
        if source_keys.iter().collect::<std::collections::HashSet<_>>().len() != source_keys.len() {
            return Err(KoraError::InvalidTransaction(
                "CLEAN token accounts must be unique".to_string(),
            ));
        }
        let expected_static_keys = source_keys.len() + if is_burn { 7 } else { 6 };
        if message.account_keys.len() != expected_static_keys {
            return Err(KoraError::InvalidTransaction(
                "CLEAN contains unrelated accounts".to_string(),
            ));
        }
        let mut addresses = source_keys.clone();
        if is_burn {
            addresses.push(
                token_instructions[0]
                    .accounts
                    .get(1)
                    .ok_or_else(|| {
                        KoraError::InvalidTransaction("CLEAN Burn mint is missing".to_string())
                    })?
                    .pubkey,
            );
        }
        let accounts = rpc_client.get_multiple_accounts(&addresses).await?;
        let mut reclaimed = 0_u64;
        for (index, account) in accounts.iter().take(source_keys.len()).enumerate() {
            let account = account.as_ref().ok_or_else(|| {
                KoraError::InvalidTransaction("CLEAN token account is missing".to_string())
            })?;
            let data = &account.data;
            let expected_amount = u64::from_le_bytes(
                data.get(64..72)
                    .ok_or_else(|| {
                        KoraError::InvalidTransaction(
                            "CLEAN token account is malformed".to_string(),
                        )
                    })?
                    .try_into()
                    .map_err(|_| KoraError::ConfigError)?,
            );
            let close_tag = data.get(129..133).ok_or_else(|| {
                KoraError::InvalidTransaction("CLEAN token account is malformed".to_string())
            })?;
            let close_valid = close_tag == [0, 0, 0, 0];
            if account.owner != token_program
                || data.len() != 165
                || data.get(32..64) != Some(wallet.as_ref())
                || data.get(72..76) != Some(&[0, 0, 0, 0])
                || data.get(108) != Some(&1)
                || data.get(109..113) != Some(&[0, 0, 0, 0])
                || data.get(121..129) != Some(&[0, 0, 0, 0, 0, 0, 0, 0])
                || !close_valid
                || (!is_burn && expected_amount != 0)
            {
                return Err(KoraError::InvalidTransaction(
                    "CLEAN token account is not eligible".to_string(),
                ));
            }
            reclaimed = reclaimed.checked_add(account.lamports).ok_or(KoraError::ConfigError)?;
            if is_burn && index == 0 {
                let burn = &token_instructions[0];
                let (amount, decimals) =
                    match spl_token_interface::instruction::TokenInstruction::unpack(&burn.data) {
                        Ok(spl_token_interface::instruction::TokenInstruction::BurnChecked {
                            amount,
                            decimals,
                        }) => (amount, decimals),
                        _ => {
                            return Err(KoraError::InvalidTransaction(
                                "CLEAN Burn must use BurnChecked for the full balance".to_string(),
                            ))
                        }
                    };
                if burn.program_id != token_program
                    || burn.accounts.len() != 3
                    || burn.accounts[0].pubkey != source_keys[0]
                    || burn.accounts[2].pubkey != wallet
                    || amount != expected_amount
                    || amount == 0
                {
                    return Err(KoraError::InvalidTransaction(
                        "CLEAN Burn fields do not match current state".to_string(),
                    ));
                }
                let mint =
                    accounts.last().and_then(|account| account.as_ref()).ok_or_else(|| {
                        KoraError::InvalidTransaction("CLEAN Burn mint is missing".to_string())
                    })?;
                if mint.owner != token_program
                    || mint.data.len() != 82
                    || mint.data[44] == 0
                    || mint.data[45] != 1
                    || decimals != mint.data[44]
                    || burn.accounts[1].pubkey != addresses[source_keys.len()]
                {
                    return Err(KoraError::InvalidTransaction(
                        "CLEAN Burn mint is not an eligible fungible mint".to_string(),
                    ));
                }
            }
        }
        let network_fee = rpc_client.get_fee_for_message(message).await?;
        let service_fee =
            reclaimed.checked_mul(u64::from(policy.fee_bps)).ok_or(KoraError::ConfigError)?
                / 10_000;
        if settlement_lamports
            != service_fee.checked_add(network_fee).ok_or(KoraError::ConfigError)?
        {
            return Err(KoraError::InvalidTransaction(
                "CLEAN settlement does not match rent, fee, and network cost".to_string(),
            ));
        }
        Ok(())
    }

    async fn validate_recover(
        &self,
        transaction: &VersionedTransactionResolved,
        rpc_client: &RpcClient,
        payer_creations: usize,
    ) -> Result<(), KoraError> {
        let policy = &self.fee_payer_policy.system.recover;
        if !policy.enabled || policy.swap_fee_bps > 10_000 || policy.rent_fee_bps > 10_000 {
            return Err(KoraError::InvalidTransaction(
                "Recover Value is disabled or invalid".to_string(),
            ));
        }
        let parse = |value: &str| Pubkey::from_str(value).map_err(|_| KoraError::ConfigError);
        let treasury = parse(&policy.settlement_wallet)?;
        let mint = parse(&policy.input_mint)?;
        let native_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")
            .map_err(|_| KoraError::ConfigError)?;
        let token_program = spl_token_interface::id();
        let ata_program = spl_associated_token_account_interface::program::id();
        let compute_program = solana_compute_budget_interface::id();
        let jupiter_program =
            Pubkey::from_str(JUPITER_V6_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let raydium_program =
            Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let message = match &transaction.transaction.message {
            solana_message::VersionedMessage::V0(message) => message,
            _ => {
                return Err(KoraError::InvalidTransaction(
                    "Recover Value requires a v0 message".to_string(),
                ))
            }
        };
        let signer_keys = transaction.transaction.message.static_account_keys();
        let wallet = signer_keys.get(1).copied().ok_or_else(|| {
            KoraError::InvalidTransaction("Recover Value user signer is missing".to_string())
        })?;
        let identity = policy
            .allowed_users
            .iter()
            .find(|identity| identity.wallet == wallet.to_string())
            .ok_or_else(|| {
                KoraError::InvalidTransaction("Recover Value user is not allowed".to_string())
            })?;
        let source = parse(&identity.source_account)?;
        let wrapped = parse(&identity.wrapped_sol_account)?;
        if wallet == self.fee_payer_pubkey || wallet == treasury || treasury == self.fee_payer_pubkey || source != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&wallet, &mint, &token_program) || wrapped != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&wallet, &native_mint, &token_program) {
            return Err(KoraError::InvalidTransaction("Recover Value identities or canonical accounts are invalid".to_string()));
        }
        if transaction.transaction.message.header().num_required_signatures != 2
            || transaction.transaction.message.header().num_readonly_signed_accounts != 0
            || signer_keys.first() != Some(&self.fee_payer_pubkey)
            || signer_keys.get(1) != Some(&wallet)
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value requires exactly payer and configured user signers".to_string(),
            ));
        }
        if policy.route_policy == "exact_snapshot" {
            let configured_luts = policy
                .allowed_lookup_tables
                .iter()
                .map(|value| parse(value))
                .collect::<Result<Vec<_>, _>>()?;
            let actual_luts = message
                .address_table_lookups
                .iter()
                .map(|lookup| lookup.account_key)
                .collect::<Vec<_>>();
            if actual_luts != configured_luts {
                return Err(KoraError::InvalidTransaction(
                    "Recover Value lookup tables are not approved".to_string(),
                ));
            }
        } else if policy.route_policy != "semantic_family" {
            return Err(KoraError::ConfigError);
        }
        let outer_count = transaction.transaction.message.instructions().len();
        let outer = transaction.all_instructions.get(..outer_count).ok_or_else(|| {
            KoraError::InvalidTransaction("Recover Value instructions are unresolved".to_string())
        })?;
        let expected_compute = [
            ComputeBudgetInstruction::set_compute_unit_price(
                policy.compute_unit_price_micro_lamports,
            )
            .data,
            ComputeBudgetInstruction::set_compute_unit_limit(policy.compute_unit_limit).data,
        ];
        if outer.len() < 6
            || outer[..2].iter().zip(expected_compute.iter()).any(|(instruction, expected)| {
                instruction.program_id != compute_program
                    || !instruction.accounts.is_empty()
                    || instruction.data != *expected
            })
            || outer[2..].iter().any(|instruction| instruction.program_id == compute_program)
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value compute prefix is invalid".to_string(),
            ));
        }
        let setup = outer.get(2).is_some_and(|instruction| instruction.program_id == ata_program);
        let jupiter_index = 2 + usize::from(setup);
        if outer.len() != jupiter_index + 4
            || outer[jupiter_index].program_id != jupiter_program
            || outer[jupiter_index + 1].program_id != token_program
            || outer[jupiter_index + 2].program_id != token_program
            || outer[jupiter_index + 3].program_id != SYSTEM_PROGRAM_ID
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value outer instruction shape is invalid".to_string(),
            ));
        }
        let route_data = &outer[jupiter_index].data;
        if route_data.len() != 39 || route_data[..8] != [187, 100, 250, 204, 49, 196, 175, 20] {
            return Err(KoraError::InvalidTransaction(
                "Recover Value Jupiter instruction is not the approved route form".to_string(),
            ));
        }
        let input_amount =
            u64::from_le_bytes(route_data[8..16].try_into().map_err(|_| KoraError::ConfigError)?);
        let quoted_output =
            u64::from_le_bytes(route_data[16..24].try_into().map_err(|_| KoraError::ConfigError)?);
        let slippage =
            u16::from_le_bytes(route_data[24..26].try_into().map_err(|_| KoraError::ConfigError)?);
        if input_amount == 0
            || quoted_output == 0
            || slippage != policy.slippage_bps
            || route_data[26] != 0
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value amount, slippage, or platform fee is invalid".to_string(),
            ));
        }
        let minimum_output = quoted_output
            .checked_mul(u64::from(10_000 - slippage))
            .and_then(|value| value.checked_add(9_999))
            .ok_or(KoraError::ConfigError)?
            / 10_000;
        if minimum_output < policy.catastrophe_output_lamports {
            return Err(KoraError::InvalidTransaction(
                "Recover Value minimum output is below the catastrophe bound".to_string(),
            ));
        }
        let close_source = &outer[jupiter_index + 1];
        let close_wrapped = &outer[jupiter_index + 2];
        for (instruction, expected_source) in [(close_source, source), (close_wrapped, wrapped)] {
            if !matches!(
                spl_token_interface::instruction::TokenInstruction::unpack(&instruction.data),
                Ok(spl_token_interface::instruction::TokenInstruction::CloseAccount)
            ) || instruction.accounts.len() != 3
                || instruction.accounts[0].pubkey != expected_source
                || instruction.accounts[1].pubkey != wallet
                || instruction.accounts[2].pubkey != wallet
            {
                return Err(KoraError::InvalidTransaction(
                    "Recover Value cleanup is invalid".to_string(),
                ));
            }
        }
        let settlement = &outer[jupiter_index + 3];
        let settlement_lamports = match bincode::deserialize::<SystemInstruction>(&settlement.data)
        {
            Ok(SystemInstruction::Transfer { lamports }) => lamports,
            _ => {
                return Err(KoraError::InvalidTransaction(
                    "Recover Value settlement is invalid".to_string(),
                ))
            }
        };
        if settlement.accounts.len() != 2
            || settlement.accounts[0].pubkey != wallet
            || settlement.accounts[1].pubkey != treasury
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value settlement accounts are invalid".to_string(),
            ));
        }
        let raydium_contexts = transaction
            .inner_instruction_contexts
            .iter()
            .filter(|context| context.instruction.program_id == raydium_program)
            .collect::<Vec<_>>();
        if raydium_contexts.len() != 1
            || raydium_contexts[0].outer_instruction_index as usize != jupiter_index
            || raydium_contexts[0].stack_height != Some(2)
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value requires exactly one direct Jupiter to Raydium CLMM CPI".to_string(),
            ));
        }
        let raydium = &raydium_contexts[0].instruction;
        let pool_is_approved = if policy.route_policy == "exact_snapshot" {
            let approved_pools = policy
                .approved_pool_accounts
                .iter()
                .map(|value| parse(value))
                .collect::<Result<Vec<_>, _>>()?;
            raydium.accounts.get(2).is_some_and(|account| approved_pools.contains(&account.pubkey))
        } else {
            true
        };
        let token_2022_program = spl_token_2022_interface::id();
        let memo_program = Pubkey::from_str("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr")
            .map_err(|_| KoraError::ConfigError)?;
        let tick_accounts_start = if raydium.data.len() == 41
            && raydium.data[..8] == RAYDIUM_SWAP_DISCRIMINATOR
            && (12..=13).contains(&raydium.accounts.len())
        {
            9
        } else if raydium.data.len() == 41
            && raydium.data[..8] == RAYDIUM_SWAP_V2_DISCRIMINATOR
            && (16..=17).contains(&raydium.accounts.len())
            && raydium.accounts[9].pubkey == token_2022_program
            && raydium.accounts[10].pubkey == memo_program
            && raydium.accounts[11].pubkey == mint
            && raydium.accounts[12].pubkey == native_mint
        {
            13
        } else {
            return Err(KoraError::InvalidTransaction(
                "Recover Value Raydium CLMM instruction variant or account shape is invalid"
                    .to_string(),
            ));
        };
        if raydium.accounts[0].pubkey != wallet
            || raydium.accounts[2].pubkey == self.fee_payer_pubkey
            || raydium.accounts[3].pubkey != source
            || raydium.accounts[4].pubkey != wrapped
            || raydium.accounts[8].pubkey != token_program
            || !pool_is_approved
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value Raydium CLMM instruction shape is invalid".to_string(),
            ));
        }
        let auxiliary_accounts = policy
            .allowed_jupiter_auxiliary_accounts
            .iter()
            .map(|value| parse(value))
            .collect::<Result<HashSet<_>, _>>()?;
        let mut allowed_jupiter_accounts = HashSet::from([
            wallet,
            source,
            wrapped,
            mint,
            native_mint,
            token_program,
            SYSTEM_PROGRAM_ID,
            jupiter_program,
            raydium_program,
        ]);
        allowed_jupiter_accounts.extend(auxiliary_accounts);
        allowed_jupiter_accounts.extend(raydium.accounts.iter().map(|account| account.pubkey));
        if outer[jupiter_index].accounts.iter().any(|account| {
            !allowed_jupiter_accounts.contains(&account.pubkey)
                || (account.is_signer
                    && account.pubkey != wallet
                    && account.pubkey != self.fee_payer_pubkey)
        }) || raydium.accounts.iter().any(|account| {
            !outer[jupiter_index]
                .accounts
                .iter()
                .any(|outer_account| outer_account.pubkey == account.pubkey)
        }) {
            return Err(KoraError::InvalidTransaction(
                "Recover Value Jupiter route contains an unapproved or unrelated account"
                    .to_string(),
            ));
        }
        if transaction.inner_instruction_contexts.iter().any(|context| {
            context.outer_instruction_index as usize == jupiter_index
                && context
                    .instruction
                    .accounts
                    .iter()
                    .any(|account| account.pubkey == self.fee_payer_pubkey)
        }) {
            return Err(KoraError::InvalidTransaction(
                "Recover Value route cannot use the sponsor inside a CPI".to_string(),
            ));
        }
        let mut account_addresses = vec![source, mint, wrapped];
        account_addresses.extend(
            [1_usize, 2, 5, 6, 7]
                .into_iter()
                .chain(tick_accounts_start..raydium.accounts.len())
                .map(|index| raydium.accounts[index].pubkey),
        );
        let accounts = rpc_client.get_multiple_accounts(&account_addresses).await?;
        if accounts.len() != account_addresses.len() {
            return Err(KoraError::InvalidTransaction(
                "Recover Value Raydium CLMM account state is incomplete".to_string(),
            ));
        }
        self.validate_recover_route(
            raydium,
            &accounts[3..],
            raydium_program,
            token_program,
            mint,
            native_mint,
            tick_accounts_start,
        )?;
        let source_account = accounts[0].as_ref().ok_or_else(|| {
            KoraError::InvalidTransaction("Recover Value source is missing".to_string())
        })?;
        let source_data = &source_account.data;
        if source_account.owner != token_program
            || source_data.len() != 165
            || source_data[0..32] != mint.to_bytes()
            || source_data[32..64] != wallet.to_bytes()
            || source_data[72..76] != [0, 0, 0, 0]
            || source_data[108] != 1
            || source_data[109..113] != [0, 0, 0, 0]
            || source_data[121..129] != [0, 0, 0, 0, 0, 0, 0, 0]
            || source_data[129..133] != [0, 0, 0, 0]
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value source state is ineligible".to_string(),
            ));
        }
        let authoritative_amount =
            u64::from_le_bytes(source_data[64..72].try_into().map_err(|_| KoraError::ConfigError)?);
        if authoritative_amount == 0 || input_amount != authoritative_amount {
            return Err(KoraError::InvalidTransaction(
                "Recover Value must route the full current balance".to_string(),
            ));
        }
        let mint_account = accounts[1].as_ref().ok_or_else(|| {
            KoraError::InvalidTransaction("Recover Value mint is missing".to_string())
        })?;
        if mint_account.owner != token_program
            || mint_account.data.len() != 82
            || mint_account.data[44] != policy.decimals
            || mint_account.data[45] != 1
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value mint identity is invalid".to_string(),
            ));
        }
        let setup_rent = if let Some(wrapped_account) = accounts[2].as_ref() {
            let data = &wrapped_account.data;
            if setup
                || payer_creations != 0
                || wrapped_account.owner != token_program
                || data.len() != 165
                || data[0..32] != native_mint.to_bytes()
                || data[32..64] != wallet.to_bytes()
                || data[64..72] != [0, 0, 0, 0, 0, 0, 0, 0]
                || data[72..76] != [0, 0, 0, 0]
                || data[108] != 1
                || data[109..113] != [1, 0, 0, 0]
                || data[121..129] != [0, 0, 0, 0, 0, 0, 0, 0]
                || data[129..133] != [0, 0, 0, 0]
                || u64::from_le_bytes(
                    data[113..121].try_into().map_err(|_| KoraError::ConfigError)?,
                ) != wrapped_account.lamports
            {
                return Err(KoraError::InvalidTransaction(
                    "Recover Value existing wrapped SOL state is invalid".to_string(),
                ));
            }
            0
        } else {
            if !setup || payer_creations != 1 {
                return Err(KoraError::InvalidTransaction(
                    "Recover Value canonical wrapped SOL setup is missing".to_string(),
                ));
            }
            self.validate_recover_ata_creation(
                transaction,
                rpc_client,
                jupiter_index,
                wallet,
                wrapped,
                native_mint,
            )
            .await?
        };
        let referenced = outer
            .iter()
            .flat_map(|instruction| {
                std::iter::once(instruction.program_id)
                    .chain(instruction.accounts.iter().map(|account| account.pubkey))
            })
            .chain(std::iter::once(self.fee_payer_pubkey))
            .collect::<HashSet<_>>();
        if transaction.all_account_keys.iter().copied().collect::<HashSet<_>>() != referenced {
            return Err(KoraError::InvalidTransaction(
                "Recover Value contains unrelated accounts".to_string(),
            ));
        }
        let network_fee = rpc_client.get_fee_for_message(message).await?;
        let swap_fee = minimum_output
            .checked_mul(u64::from(policy.swap_fee_bps))
            .ok_or(KoraError::ConfigError)?
            / 10_000;
        let rent_fee = source_account
            .lamports
            .checked_mul(u64::from(policy.rent_fee_bps))
            .ok_or(KoraError::ConfigError)?
            / 10_000;
        let expected_settlement = swap_fee
            .checked_add(rent_fee)
            .and_then(|value| value.checked_add(network_fee))
            .and_then(|value| value.checked_add(setup_rent))
            .ok_or(KoraError::ConfigError)?;
        let minimum_user_payout = minimum_output
            .checked_add(source_account.lamports)
            .and_then(|value| value.checked_add(setup_rent))
            .and_then(|value| value.checked_sub(expected_settlement))
            .ok_or(KoraError::ConfigError)?;
        let authorized_amount = |value: &str| {
            value.parse::<u64>().map_err(|_| {
                KoraError::InvalidTransaction("Recover authorization amount is invalid".to_string())
            })
        };
        let authorization_mismatch = if let Some(authorization) =
            transaction.recover_authorization_claims.as_ref()
        {
            authorized_amount(&authorization.input_amount_raw)? != input_amount
                || authorized_amount(&authorization.expected_output_lamports)? != quoted_output
                || authorized_amount(&authorization.minimum_output_lamports)? != minimum_output
                || authorized_amount(&authorization.minimum_user_payout_lamports)?
                    != minimum_user_payout
                || authorized_amount(&authorization.swap_fee_lamports)? != swap_fee
                || authorized_amount(&authorization.rent_fee_lamports)? != rent_fee
                || authorized_amount(&authorization.network_reimbursement_lamports)? != network_fee
                || authorized_amount(&authorization.setup_rent_reimbursement_lamports)?
                    != setup_rent
                || authorized_amount(&authorization.sponsored_cost_lamports)?
                    != network_fee.checked_add(setup_rent).ok_or(KoraError::ConfigError)?
        } else {
            false
        };
        if settlement_lamports != expected_settlement
            || network_fee.checked_add(setup_rent).ok_or(KoraError::ConfigError)?
                > self.max_allowed_lamports
            || minimum_user_payout < policy.minimum_user_payout_lamports
            || authorization_mismatch
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value settlement, payer exposure, or minimum user payout is invalid"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn validate_recover_route(
        &self,
        instruction: &Instruction,
        accounts: &[Option<Account>],
        raydium_program: Pubkey,
        token_program: Pubkey,
        input_mint: Pubkey,
        output_mint: Pubkey,
        tick_accounts_start: usize,
    ) -> Result<(), KoraError> {
        if instruction.program_id != raydium_program {
            return Err(KoraError::InvalidTransaction(
                "Recover Value route DEX family is not approved".to_string(),
            ));
        }
        self.validate_raydium_clmm_accounts(
            instruction,
            accounts,
            raydium_program,
            token_program,
            input_mint,
            output_mint,
            self.fee_payer_policy.system.recover.decimals,
            9,
            tick_accounts_start,
            "Recover Value Raydium CLMM account relationships are invalid",
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_raydium_clmm_accounts(
        &self,
        instruction: &Instruction,
        accounts: &[Option<Account>],
        raydium_program: Pubkey,
        token_program: Pubkey,
        input_mint: Pubkey,
        output_mint: Pubkey,
        input_decimals: u8,
        output_decimals: u8,
        tick_accounts_start: usize,
        error_message: &str,
    ) -> Result<(), KoraError> {
        let invalid = || KoraError::InvalidTransaction(error_message.to_string());
        if !(8..=9).contains(&accounts.len()) || accounts.iter().any(Option::is_none) {
            return Err(invalid());
        }
        let accounts = accounts.iter().map(|account| account.as_ref().unwrap()).collect::<Vec<_>>();
        let amm = accounts[0];
        let pool = accounts[1];
        let input_vault = accounts[2];
        let output_vault = accounts[3];
        let observation = accounts[4];
        let config_index = amm
            .data
            .get(9..11)
            .and_then(|value| value.try_into().ok())
            .map(u16::from_le_bytes)
            .ok_or_else(invalid)?;
        let (expected_config, config_bump) = Pubkey::find_program_address(
            &[b"amm_config", &config_index.to_be_bytes()],
            &raydium_program,
        );
        let protocol_fee_rate = u32::from_le_bytes(
            amm.data.get(43..47).and_then(|value| value.try_into().ok()).ok_or_else(invalid)?,
        );
        let trade_fee_rate = u32::from_le_bytes(
            amm.data.get(47..51).and_then(|value| value.try_into().ok()).ok_or_else(invalid)?,
        );
        let config_tick_spacing = u16::from_le_bytes(
            amm.data.get(51..53).and_then(|value| value.try_into().ok()).ok_or_else(invalid)?,
        );
        let fund_fee_rate = u32::from_le_bytes(
            amm.data.get(53..57).and_then(|value| value.try_into().ok()).ok_or_else(invalid)?,
        );
        let pool_tick_spacing = u16::from_le_bytes(
            pool.data.get(235..237).and_then(|value| value.try_into().ok()).ok_or_else(invalid)?,
        );
        let input_is_mint_0 = pool.data.get(73..105) == Some(input_mint.as_ref());
        let output_is_mint_0 = pool.data.get(73..105) == Some(output_mint.as_ref());
        let (mint_0, mint_1, vault_0, vault_1, decimals_0, decimals_1) = if input_is_mint_0 {
            (
                input_mint,
                output_mint,
                instruction.accounts[5].pubkey,
                instruction.accounts[6].pubkey,
                input_decimals,
                output_decimals,
            )
        } else if output_is_mint_0 {
            (
                output_mint,
                input_mint,
                instruction.accounts[6].pubkey,
                instruction.accounts[5].pubkey,
                output_decimals,
                input_decimals,
            )
        } else {
            return Err(invalid());
        };
        if amm.owner != raydium_program
            || amm.data.len() != 117
            || amm.data[..8] != RAYDIUM_AMM_CONFIG_DISCRIMINATOR
            || instruction.accounts[1].pubkey != expected_config
            || amm.data[8] != config_bump
            || trade_fee_rate >= 1_000_000
            || protocol_fee_rate.checked_add(fund_fee_rate).is_none_or(|fee| fee > 1_000_000)
            || config_tick_spacing == 0
            || config_tick_spacing > 1_000
            || config_tick_spacing != pool_tick_spacing
            || pool.owner != raydium_program
            || pool.data.len() != 1544
            || pool.data[..8] != RAYDIUM_POOL_DISCRIMINATOR
            || pool.data[9..41] != instruction.accounts[1].pubkey.to_bytes()
            || pool.data[73..105] != mint_0.to_bytes()
            || pool.data[105..137] != mint_1.to_bytes()
            || pool.data[137..169] != vault_0.to_bytes()
            || pool.data[169..201] != vault_1.to_bytes()
            || pool.data[201..233] != instruction.accounts[7].pubkey.to_bytes()
            || pool.data[233] != decimals_0
            || pool.data[234] != decimals_1
            || pool.data[389] & (1 << 4) != 0
            || observation.owner != raydium_program
            || observation.data.len() != 4483
            || observation.data[..8] != RAYDIUM_OBSERVATION_DISCRIMINATOR
            || observation.data[8] == 0
            || observation.data[19..51] != instruction.accounts[2].pubkey.to_bytes()
        {
            return Err(invalid());
        }
        for (vault, expected_mint) in [(input_vault, input_mint), (output_vault, output_mint)] {
            if vault.owner != token_program
                || vault.data.len() != 165
                || vault.data[..32] != expected_mint.to_bytes()
                || vault.data[32..64] != instruction.accounts[2].pubkey.to_bytes()
                || vault.data[108] != 1
            {
                return Err(invalid());
            }
        }
        let tick_spacing = pool_tick_spacing;
        let current_tick =
            i32::from_le_bytes(pool.data[269..273].try_into().map_err(|_| invalid())?);
        if tick_spacing == 0 {
            return Err(invalid());
        }
        let interval = i32::from(tick_spacing).checked_mul(60).ok_or_else(invalid)?;
        let expected_current_start = current_tick.div_euclid(interval) * interval;
        let mut starts = Vec::new();
        let mut bitmap_count = 0;
        let pool_key = instruction.accounts[2].pubkey;
        let mut seen = HashSet::new();
        for (offset, account) in accounts[5..].iter().enumerate() {
            let meta = &instruction.accounts[tick_accounts_start + offset];
            if meta.is_signer
                || !meta.is_writable
                || !seen.insert(meta.pubkey)
                || account.owner != raydium_program
            {
                return Err(invalid());
            }
            if account.data.len() == 10240 && account.data[..8] == RAYDIUM_TICK_ARRAY_DISCRIMINATOR
            {
                if account.data[8..40] != pool_key.to_bytes() {
                    return Err(invalid());
                }
                let start =
                    i32::from_le_bytes(account.data[40..44].try_into().map_err(|_| invalid())?);
                if start.rem_euclid(interval) != 0
                    || Pubkey::find_program_address(
                        &[b"tick_array", pool_key.as_ref(), &start.to_be_bytes()],
                        &raydium_program,
                    )
                    .0 != meta.pubkey
                {
                    return Err(invalid());
                }
                starts.push(start);
            } else if account.data.len() == 1832
                && account.data[..8] == RAYDIUM_BITMAP_DISCRIMINATOR
            {
                if account.data[8..40] != pool_key.to_bytes()
                    || Pubkey::find_program_address(
                        &[b"pool_tick_array_bitmap_extension", pool_key.as_ref()],
                        &raydium_program,
                    )
                    .0 != meta.pubkey
                {
                    return Err(invalid());
                }
                bitmap_count += 1;
            } else {
                return Err(invalid());
            }
        }
        if bitmap_count != 1
            || !(2..=3).contains(&starts.len())
            || starts.first().is_none_or(|start| *start < expected_current_start)
            || starts.windows(2).any(|pair| pair[1] <= pair[0])
        {
            return Err(invalid());
        }
        Ok(())
    }

    async fn validate_recover_ata_creation(
        &self,
        transaction: &VersionedTransactionResolved,
        rpc_client: &RpcClient,
        jupiter_index: usize,
        wallet: Pubkey,
        destination: Pubkey,
        native_mint: Pubkey,
    ) -> Result<u64, KoraError> {
        let token_program = spl_token_interface::id();
        let ata_program = spl_associated_token_account_interface::program::id();
        let outer = &transaction.all_instructions[2];
        if outer.program_id != ata_program
            || outer.data.as_slice() != [1]
            || outer.accounts.len() != 6
            || outer.accounts[0].pubkey != self.fee_payer_pubkey
            || outer.accounts[1].pubkey != destination
            || outer.accounts[2].pubkey != wallet
            || outer.accounts[3].pubkey != native_mint
            || outer.accounts[4].pubkey != SYSTEM_PROGRAM_ID
            || outer.accounts[5].pubkey != token_program
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value ATA creation is not canonical".to_string(),
            ));
        }
        let candidates = transaction
            .inner_instruction_contexts
            .iter()
            .filter(|context| {
                context.instruction.program_id == SYSTEM_PROGRAM_ID
                    && context.instruction.accounts.first().map(|account| account.pubkey)
                        == Some(self.fee_payer_pubkey)
            })
            .collect::<Vec<_>>();
        if candidates.len() != 1
            || candidates[0].outer_instruction_index != 2
            || candidates[0].stack_height != Some(2)
            || jupiter_index != 3
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value payer-funded creation provenance is invalid".to_string(),
            ));
        }
        let create = &candidates[0].instruction;
        let (lamports, space, owner) = match bincode::deserialize::<SystemInstruction>(&create.data)
        {
            Ok(SystemInstruction::CreateAccount { lamports, space, owner }) => {
                (lamports, space, owner)
            }
            _ => {
                return Err(KoraError::InvalidTransaction(
                    "Recover Value permits only canonical ATA CreateAccount".to_string(),
                ))
            }
        };
        let rent = rpc_client.get_minimum_balance_for_rent_exemption(165).await?;
        if create.accounts.len() != 2
            || create.accounts[1].pubkey != destination
            || owner != token_program
            || space != 165
            || lamports != rent
        {
            return Err(KoraError::InvalidTransaction(
                "Recover Value ATA rent or fields are invalid".to_string(),
            ));
        }
        Ok(rent)
    }

    async fn validate_swap_route(
        &self,
        transaction: &VersionedTransactionResolved,
        rpc_client: &RpcClient,
    ) -> Result<(), KoraError> {
        let policy = &self.fee_payer_policy.system.swap;
        if !policy.enabled {
            return Err(KoraError::InvalidTransaction("Swap is disabled".to_string()));
        }
        let parse = |value: &str| Pubkey::from_str(value).map_err(|_| KoraError::ConfigError);
        let jupiter = parse(JUPITER_V6_PROGRAM_ID)?;
        let raydium = parse(RAYDIUM_CLMM_PROGRAM_ID)?;
        let meteora = parse(METEORA_DLMM_PROGRAM_ID)?;
        let pumpswap = parse(PUMPSWAP_PROGRAM_ID)?;
        let dex_programs = [raydium, meteora, pumpswap];
        let signer_keys = transaction.transaction.message.static_account_keys();
        if transaction.transaction.message.header().num_required_signatures != 2
            || transaction.transaction.message.header().num_readonly_signed_accounts != 0
            || signer_keys.first() != Some(&self.fee_payer_pubkey)
        {
            return Err(KoraError::InvalidTransaction(
                "Swap requires exactly the configured payer and one user signer".to_string(),
            ));
        }
        let wallet = signer_keys[1];
        let outer_count = transaction.transaction.message.instructions().len();
        let outer = transaction.all_instructions.get(..outer_count).ok_or_else(|| {
            KoraError::InvalidTransaction("Swap instructions are unresolved".to_string())
        })?;
        let jupiter_indices = outer
            .iter()
            .enumerate()
            .filter_map(|(index, instruction)| (instruction.program_id == jupiter).then_some(index))
            .collect::<Vec<_>>();
        if jupiter_indices.len() != 1
            || outer.iter().any(|instruction| dex_programs.contains(&instruction.program_id))
        {
            return Err(KoraError::InvalidTransaction(
                "Swap requires one outer Jupiter route and no outer DEX instruction".to_string(),
            ));
        }
        let jupiter_index = jupiter_indices[0];
        let direct = transaction
            .inner_instruction_contexts
            .iter()
            .filter(|context| {
                context.outer_instruction_index as usize == jupiter_index
                    && context.stack_height == Some(2)
                    && dex_programs.contains(&context.instruction.program_id)
            })
            .collect::<Vec<_>>();
        if direct.len() != 1 {
            return Err(KoraError::InvalidTransaction(
                "Swap requires exactly one direct Jupiter DEX CPI".to_string(),
            ));
        }
        let dex = &direct[0].instruction;
        let family = if dex.program_id == raydium {
            "RAYDIUM_CLMM"
        } else if dex.program_id == meteora {
            "METEORA_DLMM"
        } else {
            "PUMPSWAP"
        };
        if !policy.approved_dex_families.iter().any(|approved| approved == family) {
            return Err(KoraError::InvalidTransaction(
                "Swap DEX family is not approved".to_string(),
            ));
        }
        if transaction.inner_instruction_contexts.iter().any(|context| {
            context.outer_instruction_index as usize == jupiter_index
                && (context
                    .instruction
                    .accounts
                    .iter()
                    .any(|meta| meta.pubkey == self.fee_payer_pubkey)
                    || (dex_programs.contains(&context.instruction.program_id)
                        && context.instruction.program_id != dex.program_id)
                    || (context.instruction.program_id == dex.program_id
                        && !matches!(context.stack_height, Some(2 | 3))))
        }) {
            return Err(KoraError::InvalidTransaction(
                "Swap route uses the sponsor or an unapproved DEX leg".to_string(),
            ));
        }
        if dex.accounts.iter().any(|meta| {
            !outer[jupiter_index].accounts.iter().any(|outer_meta| outer_meta.pubkey == meta.pubkey)
        }) {
            return Err(KoraError::InvalidTransaction(
                "Swap DEX accounts are not bound to the Jupiter instruction".to_string(),
            ));
        }
        match family {
            "RAYDIUM_CLMM" => self.validate_raydium_clmm(dex, rpc_client, wallet).await,
            "METEORA_DLMM" => self.validate_meteora_dlmm(dex, rpc_client, wallet).await,
            "PUMPSWAP" => self.validate_pumpswap(dex, rpc_client, wallet).await,
            _ => Err(KoraError::InvalidTransaction("Swap DEX family is not approved".to_string())),
        }
    }

    async fn validate_raydium_clmm(
        &self,
        instruction: &Instruction,
        rpc_client: &RpcClient,
        wallet: Pubkey,
    ) -> Result<(), KoraError> {
        let invalid = || {
            KoraError::InvalidTransaction("Raydium CLMM route semantics are invalid".to_string())
        };
        let program =
            Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let token_program = spl_token_interface::id();
        let token_2022_program = spl_token_2022_interface::id();
        let memo_program = Pubkey::from_str(MEMO_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let legacy = instruction.data.len() == 41
            && instruction.data[..8] == RAYDIUM_SWAP_DISCRIMINATOR
            && (12..=13).contains(&instruction.accounts.len());
        let v2 = instruction.data.len() == 41
            && instruction.data[..8] == RAYDIUM_SWAP_V2_DISCRIMINATOR
            && (16..=17).contains(&instruction.accounts.len());
        if instruction.program_id != program
            || (!legacy && !v2)
            || u64::from_le_bytes(instruction.data[8..16].try_into().map_err(|_| invalid())?) == 0
            || u64::from_le_bytes(instruction.data[16..24].try_into().map_err(|_| invalid())?) == 0
            || instruction.accounts[0].pubkey != wallet
            || instruction.accounts[8].pubkey != token_program
        {
            return Err(invalid());
        }
        let tick_start = if legacy {
            9
        } else {
            if instruction.accounts[9].pubkey != token_2022_program
                || instruction.accounts[10].pubkey != memo_program
            {
                return Err(invalid());
            }
            13
        };
        let writable = [2_usize, 3, 4, 5, 6, 7];
        if instruction.accounts.iter().enumerate().any(|(index, meta)| {
            meta.is_signer != (index == 0)
                || meta.is_writable != (writable.contains(&index) || index >= tick_start)
        }) {
            return Err(invalid());
        }
        let addresses = instruction.accounts.iter().map(|meta| meta.pubkey).collect::<Vec<_>>();
        let states = rpc_client.get_multiple_accounts(&addresses).await?;
        let state = |index: usize| states.get(index).and_then(Option::as_ref).ok_or_else(invalid);
        let input_mint =
            token_account_mint(state(3)?, token_program, wallet).ok_or_else(invalid)?;
        let output_mint = if v2 {
            instruction.accounts[12].pubkey
        } else {
            let pool = state(2)?;
            let bytes = if pool.data.get(73..105) == Some(input_mint.as_ref()) {
                pool.data.get(105..137)
            } else if pool.data.get(105..137) == Some(input_mint.as_ref()) {
                pool.data.get(73..105)
            } else {
                None
            };
            Pubkey::new_from_array(
                bytes.and_then(|value| value.try_into().ok()).ok_or_else(invalid)?,
            )
        };
        let output_state = states.get(4).and_then(Option::as_ref);
        if input_mint == output_mint
            || !self.allowed_tokens.contains(&input_mint)
            || !self.allowed_tokens.contains(&output_mint)
            || instruction.accounts[3].pubkey
                != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
                    &wallet,
                    &input_mint,
                    &token_program,
                )
            || instruction.accounts[4].pubkey
                != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
                    &wallet,
                    &output_mint,
                    &token_program,
                )
            || output_state.is_some_and(|account| {
                token_account_mint(account, token_program, wallet) != Some(output_mint)
            })
            || (v2
                && (instruction.accounts[11].pubkey != input_mint
                    || instruction.accounts[12].pubkey != output_mint))
        {
            return Err(invalid());
        }
        let mint_states = rpc_client.get_multiple_accounts(&[input_mint, output_mint]).await?;
        let input_decimals = mint_states
            .first()
            .and_then(Option::as_ref)
            .and_then(|account| legacy_mint_decimals(account, token_program))
            .ok_or_else(invalid)?;
        let output_decimals = mint_states
            .get(1)
            .and_then(Option::as_ref)
            .and_then(|account| legacy_mint_decimals(account, token_program))
            .ok_or_else(invalid)?;
        let mut semantic_accounts = vec![
            states.get(1).cloned().flatten(),
            states.get(2).cloned().flatten(),
            states.get(5).cloned().flatten(),
            states.get(6).cloned().flatten(),
            states.get(7).cloned().flatten(),
        ];
        semantic_accounts.extend(states.iter().skip(tick_start).cloned());
        self.validate_raydium_clmm_accounts(
            instruction,
            &semantic_accounts,
            program,
            token_program,
            input_mint,
            output_mint,
            input_decimals,
            output_decimals,
            tick_start,
            "Raydium CLMM route semantics are invalid",
        )
    }

    async fn validate_meteora_dlmm(
        &self,
        instruction: &Instruction,
        rpc_client: &RpcClient,
        wallet: Pubkey,
    ) -> Result<(), KoraError> {
        let invalid = || {
            KoraError::InvalidTransaction("Meteora DLMM route semantics are invalid".to_string())
        };
        let program =
            Pubkey::from_str(METEORA_DLMM_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let token_program = spl_token_interface::id();
        let memo = Pubkey::from_str(MEMO_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        if instruction.program_id != program
            || instruction.data.len() < 24
            || instruction.data.len() > 256
            || ![METEORA_SWAP_DISCRIMINATOR, METEORA_SWAP2_DISCRIMINATOR]
                .contains(&instruction.data[..8].try_into().map_err(|_| invalid())?)
            || u64::from_le_bytes(instruction.data[8..16].try_into().map_err(|_| invalid())?) == 0
            || (instruction.data[..8] == METEORA_SWAP2_DISCRIMINATOR
                && u64::from_le_bytes(instruction.data[16..24].try_into().map_err(|_| invalid())?)
                    == 0)
            || !(17..=20).contains(&instruction.accounts.len())
        {
            return Err(invalid());
        }
        let has_memo = instruction.accounts[13].pubkey == memo;
        let event_index = if has_memo { 14 } else { 13 };
        let program_index = event_index + 1;
        let bins_start = program_index + 1;
        let writable = [0_usize, 1, 2, 3, 4, 5, 8, 9, 10, program_index];
        if instruction.accounts.iter().enumerate().any(|(index, meta)| {
            meta.is_signer != (index == 10)
                || meta.is_writable != (writable.contains(&index) || index >= bins_start)
        }) || instruction.accounts[1].pubkey != program
            || instruction.accounts[9].pubkey != program
            || instruction.accounts[10].pubkey != wallet
            || instruction.accounts[11].pubkey != token_program
            || instruction.accounts[12].pubkey != token_program
            || instruction.accounts[event_index].pubkey
                != Pubkey::find_program_address(&[b"__event_authority"], &program).0
            || instruction.accounts[program_index].pubkey != program
        {
            return Err(invalid());
        }
        let addresses = instruction.accounts.iter().map(|meta| meta.pubkey).collect::<Vec<_>>();
        let states = rpc_client.get_multiple_accounts(&addresses).await?;
        let state = |index: usize| states.get(index).and_then(Option::as_ref).ok_or_else(invalid);
        let pair = state(0)?;
        let mint_x = state(6)?;
        let mint_y = state(7)?;
        if pair.owner != program
            || pair.data.len() != 904
            || pair.data[..8] != METEORA_LB_PAIR_DISCRIMINATOR
            || mint_x.owner != token_program
            || mint_x.data.len() != 82
            || mint_x.data[45] != 1
            || mint_y.owner != token_program
            || mint_y.data.len() != 82
            || mint_y.data[45] != 1
            || !self.allowed_tokens.contains(&instruction.accounts[6].pubkey)
            || !self.allowed_tokens.contains(&instruction.accounts[7].pubkey)
            || pair.data[88..120] != instruction.accounts[6].pubkey.to_bytes()
            || pair.data[120..152] != instruction.accounts[7].pubkey.to_bytes()
            || pair.data[152..184] != instruction.accounts[2].pubkey.to_bytes()
            || pair.data[184..216] != instruction.accounts[3].pubkey.to_bytes()
            || pair.data[552..584] != instruction.accounts[8].pubkey.to_bytes()
        {
            return Err(invalid());
        }
        let mut ordered_mints = [instruction.accounts[6].pubkey, instruction.accounts[7].pubkey];
        ordered_mints.sort();
        let base_key = &pair.data[784..816];
        let pair_candidates = [
            Pubkey::find_program_address(
                &[base_key, ordered_mints[0].as_ref(), ordered_mints[1].as_ref()],
                &program,
            )
            .0,
            Pubkey::find_program_address(
                &[ordered_mints[0].as_ref(), ordered_mints[1].as_ref(), &pair.data[80..82]],
                &program,
            )
            .0,
            Pubkey::find_program_address(
                &[
                    ordered_mints[0].as_ref(),
                    ordered_mints[1].as_ref(),
                    &pair.data[80..82],
                    &pair.data[8..10],
                ],
                &program,
            )
            .0,
        ];
        if !pair_candidates.contains(&instruction.accounts[0].pubkey) {
            return Err(invalid());
        }
        for (index, mint) in
            [(2, instruction.accounts[6].pubkey), (3, instruction.accounts[7].pubkey)]
        {
            let vault = state(index)?;
            if instruction.accounts[index].pubkey
                != Pubkey::find_program_address(
                    &[instruction.accounts[0].pubkey.as_ref(), mint.as_ref()],
                    &program,
                )
                .0
                || !valid_legacy_token_account(
                    vault,
                    token_program,
                    mint,
                    instruction.accounts[0].pubkey,
                )
            {
                return Err(invalid());
            }
        }
        let user_in = state(4)?;
        let input_mint = token_account_mint(user_in, token_program, wallet).ok_or_else(invalid)?;
        let output_mint = if input_mint == instruction.accounts[6].pubkey {
            instruction.accounts[7].pubkey
        } else if input_mint == instruction.accounts[7].pubkey {
            instruction.accounts[6].pubkey
        } else {
            return Err(invalid());
        };
        let user_out = states.get(5).and_then(Option::as_ref);
        if input_mint == output_mint
            || !ordered_mints.contains(&input_mint)
            || !ordered_mints.contains(&output_mint)
            || instruction.accounts[4].pubkey != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&wallet, &input_mint, &token_program)
            || instruction.accounts[5].pubkey != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&wallet, &output_mint, &token_program)
            || user_out.is_some_and(|account| token_account_mint(account, token_program, wallet) != Some(output_mint))
        {
            return Err(invalid());
        }
        let oracle = state(8)?;
        if instruction.accounts[8].pubkey
            != Pubkey::find_program_address(
                &[b"oracle", instruction.accounts[0].pubkey.as_ref()],
                &program,
            )
            .0
            || oracle.owner != program
            || oracle.data.len() != 3232
            || oracle.data[..8] != METEORA_ORACLE_DISCRIMINATOR
        {
            return Err(invalid());
        }
        if instruction.accounts[1].pubkey != program {
            let bitmap = state(1)?;
            if instruction.accounts[1].pubkey
                != Pubkey::find_program_address(
                    &[b"bitmap", instruction.accounts[0].pubkey.as_ref()],
                    &program,
                )
                .0
                || bitmap.owner != program
                || bitmap.data.get(..8) != Some(METEORA_BITMAP_DISCRIMINATOR.as_slice())
            {
                return Err(invalid());
            }
        }
        let mut bin_indexes = Vec::new();
        for index in bins_start..instruction.accounts.len() {
            let bin = state(index)?;
            if bin.owner != program
                || bin.data.len() != 10136
                || bin.data[..8] != METEORA_BIN_ARRAY_DISCRIMINATOR
                || bin.data[24..56] != instruction.accounts[0].pubkey.to_bytes()
            {
                return Err(invalid());
            }
            let bin_index = i64::from_le_bytes(bin.data[8..16].try_into().map_err(|_| invalid())?);
            if instruction.accounts[index].pubkey
                != Pubkey::find_program_address(
                    &[
                        b"bin_array",
                        instruction.accounts[0].pubkey.as_ref(),
                        &bin_index.to_le_bytes(),
                    ],
                    &program,
                )
                .0
            {
                return Err(invalid());
            }
            bin_indexes.push(bin_index);
        }
        if !(2..=4).contains(&bin_indexes.len())
            || bin_indexes.windows(2).any(|pair| pair[0] == pair[1])
        {
            return Err(invalid());
        }
        Ok(())
    }

    async fn validate_pumpswap(
        &self,
        instruction: &Instruction,
        rpc_client: &RpcClient,
        wallet: Pubkey,
    ) -> Result<(), KoraError> {
        let invalid =
            || KoraError::InvalidTransaction("PumpSwap route semantics are invalid".to_string());
        let program = Pubkey::from_str(PUMPSWAP_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let fee_program =
            Pubkey::from_str(PUMP_FEE_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let token_program = spl_token_interface::id();
        let ata_program = spl_associated_token_account_interface::program::id();
        let discriminator = instruction.data.get(..8).ok_or_else(invalid)?;
        let is_sell = discriminator == PUMPSWAP_SELL_DISCRIMINATOR;
        let is_buy = discriminator == PUMPSWAP_BUY_DISCRIMINATOR
            || discriminator == PUMPSWAP_BUY_EXACT_QUOTE_DISCRIMINATOR;
        let valid_length = if is_sell {
            matches!(instruction.accounts.len(), 23 | 24)
        } else if is_buy {
            instruction.accounts.len() == 26
        } else {
            return Err(invalid());
        };
        if instruction.program_id != program
            || !valid_length
            || instruction.data.len() != if is_buy { 25 } else { 24 }
            || u64::from_le_bytes(instruction.data[8..16].try_into().map_err(|_| invalid())?) == 0
            || u64::from_le_bytes(instruction.data[16..24].try_into().map_err(|_| invalid())?) == 0
        {
            return Err(invalid());
        }
        let writable = if is_sell {
            vec![0, 1, 5, 6, 7, 8, 10, 17, instruction.accounts.len() - 1]
        } else {
            vec![0, 1, 5, 6, 7, 8, 10, 17, 20, 25]
        };
        if instruction.accounts.iter().enumerate().any(|(index, meta)| {
            meta.is_signer != (index == 1) || meta.is_writable != writable.contains(&index)
        }) || instruction.accounts[1].pubkey != wallet
            || instruction.accounts[2].pubkey
                != Pubkey::from_str(PUMP_GLOBAL_CONFIG).map_err(|_| KoraError::ConfigError)?
            || instruction.accounts[11].pubkey != token_program
            || instruction.accounts[12].pubkey != token_program
            || instruction.accounts[13].pubkey != SYSTEM_PROGRAM_ID
            || instruction.accounts[14].pubkey != ata_program
            || instruction.accounts[15].pubkey
                != Pubkey::find_program_address(&[b"__event_authority"], &program).0
            || instruction.accounts[16].pubkey != program
            || instruction.accounts[20 + usize::from(is_buy) * 2].pubkey != fee_program
        {
            return Err(invalid());
        }
        let addresses = instruction.accounts.iter().map(|meta| meta.pubkey).collect::<Vec<_>>();
        let states = rpc_client.get_multiple_accounts(&addresses).await?;
        let state = |index: usize| states.get(index).and_then(Option::as_ref).ok_or_else(invalid);
        let pool = state(0)?;
        let global = state(2)?;
        let base_mint = state(3)?;
        let quote_mint = state(4)?;
        if pool.owner != program
            || pool.data.len() != 300
            || pool.data[..8] != PUMPSWAP_POOL_DISCRIMINATOR
            || global.owner != program
            || global.data.len() != 940
            || global.data[..8] != PUMPSWAP_GLOBAL_DISCRIMINATOR
            || legacy_mint_decimals(base_mint, token_program).is_none()
            || legacy_mint_decimals(quote_mint, token_program).is_none()
            || pool.data[43..75] != instruction.accounts[3].pubkey.to_bytes()
            || pool.data[75..107] != instruction.accounts[4].pubkey.to_bytes()
            || pool.data[139..171] != instruction.accounts[7].pubkey.to_bytes()
            || pool.data[171..203] != instruction.accounts[8].pubkey.to_bytes()
            || !self.allowed_tokens.contains(&instruction.accounts[3].pubkey)
            || !self.allowed_tokens.contains(&instruction.accounts[4].pubkey)
        {
            return Err(invalid());
        }
        let index = &pool.data[9..11];
        let creator = &pool.data[11..43];
        if Pubkey::find_program_address(
            &[
                b"pool",
                index,
                creator,
                instruction.accounts[3].pubkey.as_ref(),
                instruction.accounts[4].pubkey.as_ref(),
            ],
            &program,
        )
        .0 != instruction.accounts[0].pubkey
        {
            return Err(invalid());
        }
        for (index, mint) in
            [(7, instruction.accounts[3].pubkey), (8, instruction.accounts[4].pubkey)]
        {
            let vault = state(index)?;
            if instruction.accounts[index].pubkey != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&instruction.accounts[0].pubkey, &mint, &token_program)
                || !valid_legacy_token_account(vault, token_program, mint, instruction.accounts[0].pubkey)
            { return Err(invalid()); }
        }
        let (source_index, destination_index) = if is_sell { (5, 6) } else { (6, 5) };
        for (index, mint) in
            [(5, instruction.accounts[3].pubkey), (6, instruction.accounts[4].pubkey)]
        {
            if instruction.accounts[index].pubkey != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&wallet, &mint, &token_program)
                || (index == source_index && !valid_legacy_token_account(state(index)?, token_program, mint, wallet))
                || (index == destination_index && states.get(index).and_then(Option::as_ref).is_some_and(|account| !valid_legacy_token_account(account, token_program, mint, wallet)))
            { return Err(invalid()); }
        }
        let protocol_recipient = instruction.accounts[9].pubkey;
        if !(0..8).any(|index| global.data[57 + index * 32..89 + index * 32] == protocol_recipient.to_bytes())
            || instruction.accounts[10].pubkey != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&protocol_recipient, &instruction.accounts[4].pubkey, &token_program)
            || !valid_legacy_token_account(state(10)?, token_program, instruction.accounts[4].pubkey, protocol_recipient)
        { return Err(invalid()); }
        let creator_authority =
            Pubkey::find_program_address(&[b"creator_vault", &pool.data[211..243]], &program).0;
        if instruction.accounts[18].pubkey != creator_authority
            || instruction.accounts[17].pubkey != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&creator_authority, &instruction.accounts[4].pubkey, &token_program)
            || !valid_legacy_token_account(state(17)?, token_program, instruction.accounts[4].pubkey, creator_authority)
        { return Err(invalid()); }
        let fee_config_index = if is_sell { 19 } else { 21 };
        if instruction.accounts[fee_config_index].pubkey
            != Pubkey::find_program_address(&[b"fee_config", program.as_ref()], &fee_program).0
            || state(fee_config_index)?.owner != fee_program
        {
            return Err(invalid());
        }
        if is_buy {
            let global_volume = state(19)?;
            let user_volume = state(20)?;
            if instruction.accounts[19].pubkey
                != Pubkey::find_program_address(&[b"global_volume_accumulator"], &program).0
                || global_volume.owner != program
                || global_volume.data.len() != 544
                || global_volume.data[..8] != PUMPSWAP_GLOBAL_VOLUME_DISCRIMINATOR
                || instruction.accounts[20].pubkey
                    != Pubkey::find_program_address(
                        &[b"user_volume_accumulator", wallet.as_ref()],
                        &program,
                    )
                    .0
                || user_volume.owner != program
                || user_volume.data.len() != 90
                || user_volume.data[..8] != PUMPSWAP_USER_VOLUME_DISCRIMINATOR
                || user_volume.data[8..40] != wallet.to_bytes()
            {
                return Err(invalid());
            }
        }
        let (pool_v2_index, fee_recipient_index) = if is_sell && instruction.accounts.len() == 23 {
            (None, 21)
        } else if is_sell {
            (Some(21), 22)
        } else {
            (Some(23), 24)
        };
        if pool_v2_index.is_some_and(|index| {
            instruction.accounts[index].pubkey
                != Pubkey::find_program_address(
                    &[b"pool-v2", instruction.accounts[3].pubkey.as_ref()],
                    &program,
                )
                .0
        }) {
            return Err(invalid());
        }
        let current_fee_recipients = [
            "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD",
            "9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7",
            "GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL",
            "3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR",
            "5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6",
            "EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL",
            "5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD",
            "A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW",
        ]
        .into_iter()
        .map(|value| Pubkey::from_str(value).map_err(|_| KoraError::ConfigError))
        .collect::<Result<Vec<_>, _>>()?;
        let fee_recipient = instruction.accounts[fee_recipient_index].pubkey;
        if !current_fee_recipients.contains(&fee_recipient)
            || instruction.accounts[fee_recipient_index + 1].pubkey != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&fee_recipient, &instruction.accounts[4].pubkey, &token_program)
            || !valid_legacy_token_account(state(fee_recipient_index + 1)?, token_program, instruction.accounts[4].pubkey, fee_recipient)
        { return Err(invalid()); }
        Ok(())
    }

    async fn validate_canonical_ata_creation(
        &self,
        transaction: &VersionedTransactionResolved,
        rpc_client: &RpcClient,
        payer_creations: usize,
    ) -> Result<(), KoraError> {
        let policy = &self.fee_payer_policy.system.canonical_ata_creation;
        if !policy.enabled || payer_creations != 1 {
            return Err(KoraError::InvalidTransaction(
                "Fee payer cannot be used for 'System Create Account'".to_string(),
            ));
        }

        let token_program = spl_token_interface::id();
        let ata_program = spl_associated_token_account_interface::program::id();
        let allowed_mints = policy
            .allowed_output_mints
            .iter()
            .map(|mint| Pubkey::from_str(mint))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| KoraError::ConfigError)?;
        if allowed_mints.is_empty() {
            return Err(KoraError::ConfigError);
        }

        let signer_count =
            transaction.transaction.message.header().num_required_signatures as usize;
        let signer_keys = transaction.transaction.message.static_account_keys();
        if signer_count != 2 || signer_keys.first() != Some(&self.fee_payer_pubkey) {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA creation requires exactly the configured payer and GASLESS user signers"
                    .to_string(),
            ));
        }
        let wallet = signer_keys[1];

        let jupiter_program =
            Pubkey::from_str(JUPITER_V6_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let approved_dex_programs = [
            Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?,
            Pubkey::from_str(METEORA_DLMM_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?,
            Pubkey::from_str(PUMPSWAP_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?,
        ];
        let outer_instruction_count = transaction.transaction.message.instructions().len();
        let outer_ata_indices = transaction
            .all_instructions
            .iter()
            .take(outer_instruction_count)
            .enumerate()
            .filter_map(|(index, instruction)| {
                (instruction.program_id == ata_program).then_some(index)
            })
            .collect::<Vec<_>>();
        if outer_ata_indices.len() != 1 {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA exception requires exactly one outer ATA instruction".to_string(),
            ));
        }
        let outer_jupiter_indices = transaction
            .all_instructions
            .iter()
            .take(outer_instruction_count)
            .enumerate()
            .filter_map(|(index, instruction)| {
                (instruction.program_id == jupiter_program).then_some(index)
            })
            .collect::<Vec<_>>();
        if outer_jupiter_indices.len() != 1 {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA exception requires one outer Jupiter v6 swap".to_string(),
            ));
        }
        let jupiter_index = outer_jupiter_indices[0];
        let direct_dex_contexts = transaction
            .inner_instruction_contexts
            .iter()
            .filter(|context| {
                approved_dex_programs.contains(&context.instruction.program_id)
                    && context.outer_instruction_index as usize == jupiter_index
                    && context.stack_height == Some(2)
            })
            .collect::<Vec<_>>();
        if direct_dex_contexts.len() != 1
            || transaction
                .all_instructions
                .iter()
                .take(outer_instruction_count)
                .any(|instruction| approved_dex_programs.contains(&instruction.program_id))
        {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA exception requires one direct approved Jupiter DEX CPI".to_string(),
            ));
        }

        let candidates = transaction
            .inner_instruction_contexts
            .iter()
            .filter(|context| {
                context.instruction.program_id == SYSTEM_PROGRAM_ID
                    && context.instruction.accounts.first().map(|account| account.pubkey)
                        == Some(self.fee_payer_pubkey)
            })
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA exception permits exactly one payer-funded account creation"
                    .to_string(),
            ));
        }
        let context = candidates[0];
        if context.stack_height != Some(2) {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA account creation is not a direct CPI child".to_string(),
            ));
        }
        let outer = transaction
            .all_instructions
            .get(context.outer_instruction_index as usize)
            .ok_or_else(|| {
                KoraError::InvalidTransaction(
                    "Canonical ATA parent instruction could not be proven".to_string(),
                )
            })?;
        if outer.program_id != ata_program
            || outer.data.as_slice() != [1]
            || outer.accounts.len() != 6
        {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA parent must be CreateIdempotent".to_string(),
            ));
        }
        let payer = outer.accounts[0].pubkey;
        let destination = outer.accounts[1].pubkey;
        let outer_wallet = outer.accounts[2].pubkey;
        let mint = outer.accounts[3].pubkey;
        if payer != self.fee_payer_pubkey
            || outer_wallet != wallet
            || !allowed_mints.contains(&mint)
            || outer.accounts[4].pubkey != SYSTEM_PROGRAM_ID
            || outer.accounts[5].pubkey != token_program
            || destination
                != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
                    &wallet,
                    &mint,
                    &token_program,
                )
        {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA parent accounts do not match GASLESS policy".to_string(),
            ));
        }

        let (lamports, space, owner) =
            match bincode::deserialize::<SystemInstruction>(&context.instruction.data) {
                Ok(SystemInstruction::CreateAccount { lamports, space, owner }) => {
                    (lamports, space, owner)
                }
                _ => {
                    return Err(KoraError::InvalidTransaction(
                        "Canonical ATA exception does not permit this System instruction"
                            .to_string(),
                    ))
                }
            };
        if context.instruction.accounts.len() != 2
            || context.instruction.accounts[1].pubkey != destination
            || owner != token_program
            || space != 165
        {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA CreateAccount fields are invalid".to_string(),
            ));
        }
        let rent = rpc_client.get_minimum_balance_for_rent_exemption(165).await?;
        if lamports != rent {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA CreateAccount rent is invalid".to_string(),
            ));
        }
        let existing = rpc_client.get_multiple_accounts(&[destination]).await?;
        if existing.first().and_then(|account| account.as_ref()).is_some() {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA destination already exists".to_string(),
            ));
        }
        Ok(())
    }

    async fn validate_send(
        &self,
        transaction: &VersionedTransactionResolved,
        rpc_client: &RpcClient,
        payer_creations: usize,
    ) -> Result<(), KoraError> {
        let policy = &self.fee_payer_policy.system.send;
        if !policy.enabled || payer_creations > 1 {
            return Err(KoraError::InvalidTransaction(
                "Fee payer cannot be used for 'System Create Account'".to_string(),
            ));
        }

        let token_program = spl_token_interface::id();
        let ata_program = spl_associated_token_account_interface::program::id();
        let compute_program = solana_compute_budget_interface::id();
        let jupiter_program =
            Pubkey::from_str(JUPITER_V6_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let raydium_program =
            Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
        let settlement_wallet =
            Pubkey::from_str(&policy.settlement_wallet).map_err(|_| KoraError::ConfigError)?;
        if settlement_wallet == self.fee_payer_pubkey {
            return Err(KoraError::InvalidTransaction(
                "SEND treasury and fee payer must be distinct".to_string(),
            ));
        }

        let signer_count =
            transaction.transaction.message.header().num_required_signatures as usize;
        let signer_keys = transaction.transaction.message.static_account_keys();
        if signer_count != 2 || signer_keys.first() != Some(&self.fee_payer_pubkey) {
            return Err(KoraError::InvalidTransaction(
                "SEND ATA creation requires exactly the configured payer and user signers"
                    .to_string(),
            ));
        }
        let sender = signer_keys[1];
        if sender == self.fee_payer_pubkey || sender == settlement_wallet {
            return Err(KoraError::InvalidTransaction(
                "SEND identities must be distinct".to_string(),
            ));
        }

        let outer_count = transaction.transaction.message.instructions().len();
        let outer = &transaction.all_instructions[..outer_count];
        if transaction.all_instructions.iter().any(|instruction| {
            instruction.program_id == jupiter_program || instruction.program_id == raydium_program
        }) {
            return Err(KoraError::InvalidTransaction(
                "SEND ATA creation does not permit swap programs".to_string(),
            ));
        }
        let send_index = outer
            .iter()
            .position(|instruction| instruction.program_id != compute_program)
            .ok_or_else(|| {
                KoraError::InvalidTransaction("SEND instructions are missing".to_string())
            })?;
        let expected_compute_data = [
            ComputeBudgetInstruction::set_compute_unit_price(SEND_COMPUTE_UNIT_PRICE_MICROLAMPORTS)
                .data,
            ComputeBudgetInstruction::set_compute_unit_limit(SEND_COMPUTE_UNIT_LIMIT).data,
        ];
        if send_index != expected_compute_data.len()
            || outer[..send_index].iter().zip(expected_compute_data.iter()).any(
                |(instruction, expected_data)| {
                    instruction.program_id != compute_program
                        || !instruction.accounts.is_empty()
                        || instruction.data != *expected_data
                },
            )
        {
            return Err(KoraError::InvalidTransaction(
                "SEND requires the exact bounded compute-budget prefix".to_string(),
            ));
        }
        let creates_recipient_ata = outer[send_index].program_id == ata_program;
        let transfer_index = send_index + usize::from(creates_recipient_ata);
        if outer.len() != transfer_index + 3
            || outer[transfer_index..]
                .iter()
                .any(|instruction| instruction.program_id != token_program)
            || creates_recipient_ata != (payer_creations == 1)
        {
            return Err(KoraError::InvalidTransaction(
                "SEND requires exactly three token transfers and at most one recipient ATA creation"
                    .to_string(),
            ));
        }

        let first_transfer = &outer[transfer_index];
        if first_transfer.accounts.len() != 4 {
            return Err(KoraError::InvalidTransaction(
                "SEND recipient transfer accounts are invalid".to_string(),
            ));
        }
        let mint = first_transfer.accounts[1].pubkey;
        let destination = first_transfer.accounts[2].pubkey;
        let approved = policy
            .approved_mints
            .iter()
            .find(|approved| approved.mint == mint.to_string())
            .ok_or_else(|| {
                KoraError::InvalidTransaction("SEND mint is not approved".to_string())
            })?;
        if !self.allowed_tokens.contains(&mint) {
            return Err(KoraError::InvalidTransaction(
                "SEND mint is not globally allowed".to_string(),
            ));
        }
        let settlement = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
            &settlement_wallet,
            &mint,
            &token_program,
        );

        let recipient_from_creation = if creates_recipient_ata {
            let ata = &outer[send_index];
            if ata.data.as_slice() != [1]
                || ata.accounts.len() != 6
                || ata.accounts[0].pubkey != self.fee_payer_pubkey
                || ata.accounts[1].pubkey != destination
                || ata.accounts[3].pubkey != mint
                || ata.accounts[4].pubkey != SYSTEM_PROGRAM_ID
                || ata.accounts[5].pubkey != token_program
            {
                return Err(KoraError::InvalidTransaction(
                    "SEND ATA parent must be an exact CreateIdempotent".to_string(),
                ));
            }
            Some(ata.accounts[2].pubkey)
        } else {
            None
        };

        let payer_system_actions = transaction
            .inner_instruction_contexts
            .iter()
            .filter(|context| {
                context.instruction.program_id == SYSTEM_PROGRAM_ID
                    && context.instruction.accounts.first().map(|account| account.pubkey)
                        == Some(self.fee_payer_pubkey)
            })
            .collect::<Vec<_>>();
        if payer_system_actions.len() != usize::from(creates_recipient_ata) {
            return Err(KoraError::InvalidTransaction(
                "SEND permits only its one payer-funded recipient ATA creation".to_string(),
            ));
        }
        if let Some(creation) = payer_system_actions.first() {
            if creation.outer_instruction_index as usize != send_index
                || creation.stack_height != Some(2)
                || creation.instruction.accounts.len() != 2
                || creation.instruction.accounts[1].pubkey != destination
            {
                return Err(KoraError::InvalidTransaction(
                    "SEND ATA CreateAccount provenance is invalid".to_string(),
                ));
            }
            let (lamports, space, owner) =
                match bincode::deserialize::<SystemInstruction>(&creation.instruction.data) {
                    Ok(SystemInstruction::CreateAccount { lamports, space, owner }) => {
                        (lamports, space, owner)
                    }
                    _ => {
                        return Err(KoraError::InvalidTransaction(
                            "SEND ATA creation does not permit this System instruction".to_string(),
                        ))
                    }
                };
            let rent = rpc_client.get_minimum_balance_for_rent_exemption(165).await?;
            if owner != token_program || space != 165 || lamports != rent {
                return Err(KoraError::InvalidTransaction(
                    "SEND ATA CreateAccount fields are invalid".to_string(),
                ));
            }
        }

        let source = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
            &sender,
            &mint,
            &token_program,
        );
        let expected_destinations = [destination, settlement, settlement];
        let mut total_debit = 0_u64;
        for (index, instruction) in outer[transfer_index..].iter().enumerate() {
            let (amount, decimals) =
                match spl_token_interface::instruction::TokenInstruction::unpack(&instruction.data)
                {
                    Ok(spl_token_interface::instruction::TokenInstruction::TransferChecked {
                        amount,
                        decimals,
                    }) => (amount, decimals),
                    _ => {
                        return Err(KoraError::InvalidTransaction(
                            "SEND requires TransferChecked instructions".to_string(),
                        ))
                    }
                };
            if amount == 0
                || decimals != approved.decimals
                || instruction.accounts.len() != 4
                || instruction.accounts[0].pubkey != source
                || instruction.accounts[1].pubkey != mint
                || instruction.accounts[2].pubkey != expected_destinations[index]
                || instruction.accounts[3].pubkey != sender
            {
                return Err(KoraError::InvalidTransaction(
                    "SEND transfer fields do not match policy".to_string(),
                ));
            }
            total_debit = total_debit.checked_add(amount).ok_or_else(|| {
                KoraError::InvalidTransaction("SEND token debit overflow".to_string())
            })?;
        }
        if transaction.inner_instruction_contexts.iter().any(|context| {
            context.instruction.program_id == token_program
                && matches!(
                    spl_token_interface::instruction::TokenInstruction::unpack(
                        &context.instruction.data
                    ),
                    Ok(spl_token_interface::instruction::TokenInstruction::Transfer { .. })
                        | Ok(spl_token_interface::instruction::TokenInstruction::TransferChecked {
                            ..
                        })
                )
        }) {
            return Err(KoraError::InvalidTransaction(
                "SEND does not permit inner token transfers".to_string(),
            ));
        }

        let accounts = rpc_client.get_multiple_accounts(&[destination, source, settlement]).await?;
        let destination_account = accounts.first().and_then(|account| account.as_ref());
        if destination_account.is_some() == creates_recipient_ata {
            return Err(KoraError::InvalidTransaction(
                "SEND recipient ATA existence does not match the transaction".to_string(),
            ));
        }
        let source_account =
            accounts.get(1).and_then(|account| account.as_ref()).ok_or_else(|| {
                KoraError::InvalidTransaction("SEND source ATA is missing".to_string())
            })?;
        let settlement_account =
            accounts.get(2).and_then(|account| account.as_ref()).ok_or_else(|| {
                KoraError::InvalidTransaction("SEND settlement ATA is missing".to_string())
            })?;
        let token_account_valid =
            |account: &solana_sdk::account::Account, expected_owner: Option<Pubkey>| {
                account.owner == token_program
                    && account.data.len() == 165
                    && Pubkey::try_from(&account.data[0..32]).ok() == Some(mint)
                    && expected_owner
                        .map(|owner| Pubkey::try_from(&account.data[32..64]).ok() == Some(owner))
                        .unwrap_or(true)
                    && account.data[108] == 1
            };
        let recipient = match recipient_from_creation {
            Some(recipient) => recipient,
            None => {
                let account = destination_account.ok_or_else(|| {
                    KoraError::InvalidTransaction("SEND recipient ATA is missing".to_string())
                })?;
                if !token_account_valid(account, None) {
                    return Err(KoraError::InvalidTransaction(
                        "SEND recipient ATA is unhealthy".to_string(),
                    ));
                }
                Pubkey::try_from(&account.data[32..64]).map_err(|_| {
                    KoraError::InvalidTransaction("SEND recipient owner is invalid".to_string())
                })?
            }
        };
        if !recipient.is_on_curve()
            || recipient == sender
            || recipient == self.fee_payer_pubkey
            || recipient == settlement_wallet
            || destination
                != spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
                    &recipient,
                    &mint,
                    &token_program,
                )
        {
            return Err(KoraError::InvalidTransaction(
                "SEND recipient identity or canonical ATA is invalid".to_string(),
            ));
        }
        if !token_account_valid(source_account, Some(sender))
            || u64::from_le_bytes(
                source_account.data[64..72].try_into().map_err(|_| {
                    KoraError::InvalidTransaction("SEND source is invalid".to_string())
                })?,
            ) < total_debit
            || source_account.data[72..76] != [0, 0, 0, 0]
            || !token_account_valid(settlement_account, Some(settlement_wallet))
            || settlement_account.data[72..76] != [0, 0, 0, 0]
        {
            return Err(KoraError::InvalidTransaction(
                "SEND token accounts are not healthy".to_string(),
            ));
        }
        Ok(())
    }

    async fn validate_transfer_amounts(
        &self,
        transaction_resolved: &mut VersionedTransactionResolved,
        rpc_client: &RpcClient,
    ) -> Result<(), KoraError> {
        let total_outflow = self.calculate_total_outflow(transaction_resolved, rpc_client).await?;

        if total_outflow > self.max_allowed_lamports {
            return Err(KoraError::InvalidTransaction(format!(
                "Total transfer amount {} exceeds maximum allowed {}",
                total_outflow, self.max_allowed_lamports
            )));
        }

        Ok(())
    }

    fn validate_disallowed_accounts(
        &self,
        transaction_resolved: &VersionedTransactionResolved,
    ) -> Result<(), KoraError> {
        for instruction in &transaction_resolved.all_instructions {
            if self.disallowed_accounts.contains(&instruction.program_id) {
                return Err(KoraError::InvalidTransaction(format!(
                    "Program {} is disallowed",
                    instruction.program_id
                )));
            }

            for account_index in instruction.accounts.iter() {
                if self.disallowed_accounts.contains(&account_index.pubkey) {
                    return Err(KoraError::InvalidTransaction(format!(
                        "Account {} is disallowed",
                        account_index.pubkey
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn is_disallowed_account(&self, account: &Pubkey) -> bool {
        self.disallowed_accounts.contains(account)
    }

    async fn calculate_total_outflow(
        &self,
        transaction_resolved: &mut VersionedTransactionResolved,
        rpc_client: &RpcClient,
    ) -> Result<u64, KoraError> {
        let config = get_config()?;
        FeeConfigUtil::calculate_fee_payer_outflow(
            &self.fee_payer_pubkey,
            transaction_resolved,
            rpc_client,
            &config.validation.price_source,
        )
        .await
    }

    pub async fn validate_token_payment(
        transaction_resolved: &mut VersionedTransactionResolved,
        required_lamports: u64,
        rpc_client: &RpcClient,
        expected_payment_destination: &Pubkey,
    ) -> Result<(), KoraError> {
        if TokenUtil::verify_token_payment(
            transaction_resolved,
            rpc_client,
            required_lamports,
            expected_payment_destination,
        )
        .await?
        {
            return Ok(());
        }

        Err(KoraError::InvalidTransaction(format!(
            "Insufficient token payment. Required {required_lamports} lamports"
        )))
    }

    pub fn validate_strict_pricing_with_fee(
        fee_calculation: &TotalFeeCalculation,
    ) -> Result<(), KoraError> {
        let config = get_config()?;

        if !matches!(&config.validation.price.model, PriceModel::Fixed { strict: true, .. }) {
            return Ok(());
        }

        let fixed_price_lamports = fee_calculation.total_fee_lamports;
        let total_fee_lamports = fee_calculation.get_total_fee_lamports()?;

        if fixed_price_lamports < total_fee_lamports {
            log::error!(
                "Strict pricing violation: fixed_price_lamports={} < total_fee_lamports={}",
                fixed_price_lamports,
                total_fee_lamports
            );
            return Err(KoraError::ValidationError(format!(
                    "Strict pricing violation: total fee ({} lamports) exceeds fixed price ({} lamports)",
                    total_fee_lamports,
                    fixed_price_lamports
                )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::{
            CanonicalAtaCreationPolicy, CleanPolicy, FeePayerPolicy, RecoverPolicy, SendMintPolicy,
            SendPolicy,
        },
        state::update_config,
        tests::{
            config_mock::{mock_state, ConfigMockBuilder},
            rpc_mock::RpcMockBuilder,
        },
        transaction::{InnerInstructionContext, RecoverAuthorizationClaims, TransactionUtil},
    };
    use base64::Engine;
    use serial_test::serial;

    use super::*;
    use serde_json::json;
    use solana_client::rpc_request::RpcRequest;
    use solana_message::{Message, VersionedMessage};
    use solana_sdk::{
        hash::Hash,
        instruction::{AccountMeta, Instruction},
    };
    use solana_system_interface::{
        instruction::{
            assign, create_account, create_account_with_seed, transfer, transfer_with_seed,
        },
        program::ID as SYSTEM_PROGRAM_ID,
    };
    use std::collections::HashMap;

    // Helper functions to reduce test duplication and setup config
    fn setup_default_config() {
        let config = ConfigMockBuilder::new()
            .with_price_source(PriceSource::Mock)
            .with_allowed_programs(vec![SYSTEM_PROGRAM_ID.to_string()])
            .with_max_allowed_lamports(1_000_000)
            .with_fee_payer_policy(FeePayerPolicy::default())
            .build();
        update_config(config).unwrap();
    }

    fn setup_config_with_policy(policy: FeePayerPolicy) {
        let allowed_tokens = policy
            .system
            .canonical_ata_creation
            .allowed_output_mints
            .iter()
            .cloned()
            .chain(policy.system.send.approved_mints.iter().map(|approved| approved.mint.clone()))
            .collect::<Vec<_>>();
        let mut builder = ConfigMockBuilder::new()
            .with_price_source(PriceSource::Mock)
            .with_allowed_programs(vec![SYSTEM_PROGRAM_ID.to_string()])
            .with_max_allowed_lamports(1_000_000)
            .with_fee_payer_policy(policy);
        if !allowed_tokens.is_empty() {
            builder = builder.with_allowed_tokens(allowed_tokens);
        }
        let config = builder.build();
        update_config(config).unwrap();
    }

    fn canonical_ata_fixture(
        rent: u64,
        existing: bool,
    ) -> (
        TransactionValidator,
        VersionedTransactionResolved,
        std::sync::Arc<RpcClient>,
        Pubkey,
        Pubkey,
        Pubkey,
    ) {
        let payer = Pubkey::new_unique();
        let wallet = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let token_program = spl_token_interface::id();
        let destination = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&wallet, &mint, &token_program);
        let mut policy = FeePayerPolicy::default();
        policy.system.canonical_ata_creation = CanonicalAtaCreationPolicy {
            enabled: true,
            allowed_output_mints: vec![mint.to_string()],
        };
        setup_config_with_policy(policy);
        let ata = spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(&payer, &wallet, &mint, &token_program);
        let jupiter_program = Pubkey::from_str(JUPITER_V6_PROGRAM_ID).unwrap();
        let raydium_program = Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).unwrap();
        let user_signer = Instruction::new_with_bytes(
            jupiter_program,
            &[],
            vec![AccountMeta::new_readonly(wallet, true)],
        );
        let message = VersionedMessage::Legacy(Message::new(&[ata, user_signer], Some(&payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        let create = create_account(&payer, &destination, rent, 165, &token_program);
        transaction.all_instructions.push(create.clone());
        transaction.inner_instruction_contexts.push(InnerInstructionContext {
            instruction: create,
            outer_instruction_index: 0,
            stack_height: Some(2),
        });
        let raydium = Instruction::new_with_bytes(raydium_program, &[], vec![]);
        transaction.all_instructions.push(raydium.clone());
        transaction.inner_instruction_contexts.push(InnerInstructionContext {
            instruction: raydium,
            outer_instruction_index: 1,
            stack_height: Some(2),
        });
        let mut mocks = HashMap::new();
        mocks.insert(RpcRequest::GetMinimumBalanceForRentExemption, json!(rent));
        mocks.insert(RpcRequest::GetMultipleAccounts, json!({ "context": { "slot": 1 }, "value": if existing { vec![Some(json!({ "data": [base64::engine::general_purpose::STANDARD.encode(vec![0_u8; 165]), "base64"], "executable": false, "lamports": rent, "owner": token_program.to_string(), "rentEpoch": 0 }))] } else { vec![None::<serde_json::Value>] } }));
        let rpc = RpcMockBuilder::new().with_custom_mocks(mocks).build();
        (TransactionValidator::new(payer).unwrap(), transaction, rpc, payer, wallet, mint)
    }

    fn clean_fixture(
        burn: bool,
        claim_enabled: bool,
        burn_enabled: bool,
        claim_accounts: usize,
    ) -> (
        TransactionValidator,
        VersionedTransactionResolved,
        std::sync::Arc<RpcClient>,
        Pubkey,
        Pubkey,
    ) {
        clean_fixture_with_account_mutation(burn, claim_enabled, burn_enabled, claim_accounts, None)
    }

    fn clean_fixture_with_account_mutation(
        burn: bool,
        claim_enabled: bool,
        burn_enabled: bool,
        claim_accounts: usize,
        account_mutation: Option<usize>,
    ) -> (
        TransactionValidator,
        VersionedTransactionResolved,
        std::sync::Arc<RpcClient>,
        Pubkey,
        Pubkey,
    ) {
        let payer = Pubkey::new_unique();
        let wallet = Pubkey::new_unique();
        let treasury = Pubkey::new_unique();
        let token_accounts =
            (0..claim_accounts.max(1)).map(|_| Pubkey::new_unique()).collect::<Vec<_>>();
        let mint = Pubkey::new_unique();
        let token_program = spl_token_interface::id();
        let amount = if burn { 123_456_u64 } else { 0 };
        let mut policy = FeePayerPolicy::default();
        policy.system.clean = CleanPolicy {
            claim_enabled,
            burn_enabled,
            settlement_wallet: treasury.to_string(),
            fee_bps: 300,
            maximum_claim_accounts: 10,
        };
        setup_config_with_policy(policy);
        let mut instructions = vec![
            solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_price(
                375_000,
            ),
            solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_limit(
                100_000,
            ),
        ];
        if burn {
            instructions.push(
                spl_token_interface::instruction::burn_checked(
                    &token_program,
                    &token_accounts[0],
                    &mint,
                    &wallet,
                    &[],
                    amount,
                    6,
                )
                .unwrap(),
            );
        }
        for token_account in token_accounts.iter().take(if burn { 1 } else { claim_accounts }) {
            instructions.push(
                spl_token_interface::instruction::close_account(
                    &token_program,
                    token_account,
                    &wallet,
                    &wallet,
                    &[],
                )
                .unwrap(),
            );
        }
        let reclaimed = 2_039_280_u64 * if burn { 1 } else { claim_accounts as u64 };
        instructions.push(transfer(&wallet, &treasury, reclaimed * 300 / 10_000 + 42_500));
        let message = solana_message::v0::Message::try_compile(
            &payer,
            &instructions,
            &[],
            Hash::new_unique(),
        )
        .unwrap();
        let transaction = TransactionUtil::new_unsigned_versioned_transaction_resolved(
            VersionedMessage::V0(message),
        )
        .unwrap();
        let mut token_data = vec![0_u8; 165];
        token_data[0..32].copy_from_slice(mint.as_ref());
        token_data[32..64].copy_from_slice(wallet.as_ref());
        token_data[64..72].copy_from_slice(&amount.to_le_bytes());
        token_data[108] = 1;
        let mut account_owner = token_program;
        match account_mutation {
            Some(0) => token_data[64..72].copy_from_slice(&1_u64.to_le_bytes()),
            Some(1) => token_data[32..64].copy_from_slice(Pubkey::new_unique().as_ref()),
            Some(2) => token_data[72..76].copy_from_slice(&1_u32.to_le_bytes()),
            Some(3) => token_data[108] = 2,
            Some(4) => token_data[109..113].copy_from_slice(&1_u32.to_le_bytes()),
            Some(5) => token_data[129..133].copy_from_slice(&1_u32.to_le_bytes()),
            Some(6) => account_owner = spl_token_2022_interface::id(),
            Some(7) => token_data.truncate(164),
            _ => {}
        }
        let token_json = json!({ "data": [base64::engine::general_purpose::STANDARD.encode(token_data), "base64"], "executable": false, "lamports": 2_039_280, "owner": account_owner.to_string(), "rentEpoch": 0 });
        let mut values = vec![Some(token_json); if burn { 1 } else { claim_accounts }];
        if burn {
            let mut mint_data = vec![0_u8; 82];
            mint_data[44] = 6;
            mint_data[45] = 1;
            values.push(Some(json!({ "data": [base64::engine::general_purpose::STANDARD.encode(mint_data), "base64"], "executable": false, "lamports": 1_461_600, "owner": token_program.to_string(), "rentEpoch": 0 })));
        }
        let mut mocks = HashMap::new();
        mocks.insert(
            RpcRequest::GetMultipleAccounts,
            json!({ "context": { "slot": 1 }, "value": values }),
        );
        mocks.insert(
            RpcRequest::GetFeeForMessage,
            json!({ "context": { "slot": 1 }, "value": 42_500 }),
        );
        let rpc = RpcMockBuilder::new().with_custom_mocks(mocks).build();
        (TransactionValidator::new(payer).unwrap(), transaction, rpc, wallet, treasury)
    }

    fn recover_fixture(
        existing_wrapped: bool,
        source_mutation: Option<usize>,
        wrapped_mutation: Option<usize>,
    ) -> (TransactionValidator, VersionedTransactionResolved, std::sync::Arc<RpcClient>, usize)
    {
        recover_fixture_with_output(
            existing_wrapped,
            source_mutation,
            wrapped_mutation,
            1_000_000_000,
        )
    }

    fn recover_fixture_with_output(
        existing_wrapped: bool,
        source_mutation: Option<usize>,
        wrapped_mutation: Option<usize>,
        quoted_output: u64,
    ) -> (TransactionValidator, VersionedTransactionResolved, std::sync::Arc<RpcClient>, usize)
    {
        recover_fixture_with_output_and_semantic_mutation(
            existing_wrapped,
            source_mutation,
            wrapped_mutation,
            quoted_output,
            None,
            None,
        )
    }

    #[derive(Clone, Copy)]
    struct RecoverFixtureIdentity {
        payer: Pubkey,
        wallet: Pubkey,
        treasury: Pubkey,
        mint: Pubkey,
        pool: Pubkey,
        lookup_table: Pubkey,
    }

    fn stable_recover_identity(pool: Pubkey, lookup_table: Pubkey) -> RecoverFixtureIdentity {
        RecoverFixtureIdentity {
            payer: Pubkey::new_from_array([1; 32]),
            wallet: Pubkey::new_from_array([2; 32]),
            treasury: Pubkey::new_from_array([3; 32]),
            mint: Pubkey::new_from_array([4; 32]),
            pool,
            lookup_table,
        }
    }

    fn recover_fixture_with_output_and_semantic_mutation(
        existing_wrapped: bool,
        source_mutation: Option<usize>,
        wrapped_mutation: Option<usize>,
        quoted_output: u64,
        semantic_mutation: Option<usize>,
        identity_override: Option<RecoverFixtureIdentity>,
    ) -> (TransactionValidator, VersionedTransactionResolved, std::sync::Arc<RpcClient>, usize)
    {
        let identity = identity_override.unwrap_or_else(|| RecoverFixtureIdentity {
            payer: Pubkey::new_unique(),
            wallet: Pubkey::new_unique(),
            treasury: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            pool: Pubkey::new_unique(),
            lookup_table: Pubkey::new_unique(),
        });
        let RecoverFixtureIdentity { payer, wallet, treasury, mint, pool, lookup_table } = identity;
        let token_program = spl_token_interface::id();
        let native_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        let jupiter_program = Pubkey::from_str(JUPITER_V6_PROGRAM_ID).unwrap();
        let raydium_program = Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).unwrap();
        let config_index = 0_u16;
        let (amm_config, config_bump) = Pubkey::find_program_address(
            &[b"amm_config", &config_index.to_be_bytes()],
            &raydium_program,
        );
        let input_vault = Pubkey::new_unique();
        let output_vault = Pubkey::new_unique();
        let observation = Pubkey::new_unique();
        let tick_starts = [0_i32, 60, 120];
        let tick_arrays = tick_starts.map(|start| {
            Pubkey::find_program_address(
                &[b"tick_array", pool.as_ref(), &start.to_be_bytes()],
                &raydium_program,
            )
            .0
        });
        let bitmap = Pubkey::find_program_address(
            &[b"pool_tick_array_bitmap_extension", pool.as_ref()],
            &raydium_program,
        )
        .0;
        let source = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&wallet, &mint, &token_program);
        let wrapped = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&wallet, &native_mint, &token_program);
        let input_amount = 1_779_926_u64;
        let minimum_output = quoted_output * 9_950 / 10_000;
        let network_fee = 42_500_u64;
        let source_rent = 2_039_280_u64;
        let setup_rent = 2_039_280_u64;
        let settlement = minimum_output * 30 / 10_000
            + source_rent * 300 / 10_000
            + network_fee
            + if existing_wrapped { 0 } else { setup_rent };

        let mut raydium_accounts = vec![
            AccountMeta::new_readonly(wallet, true),
            AccountMeta::new_readonly(amm_config, false),
            AccountMeta::new(pool, false),
            AccountMeta::new(source, false),
            AccountMeta::new(wrapped, false),
            AccountMeta::new(input_vault, false),
            AccountMeta::new(output_vault, false),
            AccountMeta::new(observation, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new(tick_arrays[0], false),
            AccountMeta::new(bitmap, false),
            AccountMeta::new(tick_arrays[1], false),
            AccountMeta::new(tick_arrays[2], false),
        ];
        if semantic_mutation == Some(9) {
            raydium_accounts.remove(11);
        }
        if semantic_mutation == Some(10) {
            raydium_accounts.splice(
                9..9,
                [
                    AccountMeta::new_readonly(spl_token_2022_interface::id(), false),
                    AccountMeta::new_readonly(
                        Pubkey::from_str("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr").unwrap(),
                        false,
                    ),
                    AccountMeta::new_readonly(mint, false),
                    AccountMeta::new_readonly(native_mint, false),
                ],
            );
        }
        let mut route_accounts = vec![
            AccountMeta::new(source, false),
            AccountMeta::new(wrapped, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(native_mint, false),
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(raydium_program, false),
            AccountMeta::new(wallet, true),
            AccountMeta::new_readonly(token_program, false),
        ];
        for account in &raydium_accounts {
            if !route_accounts.iter().any(|existing| existing.pubkey == account.pubkey) {
                route_accounts.push(account.clone());
            }
        }
        let mut route_data = vec![187, 100, 250, 204, 49, 196, 175, 20];
        route_data.extend_from_slice(&input_amount.to_le_bytes());
        route_data.extend_from_slice(&quoted_output.to_le_bytes());
        route_data.extend_from_slice(&50_u16.to_le_bytes());
        route_data.extend_from_slice(&[0_u8; 13]);
        let route =
            Instruction::new_with_bytes(jupiter_program, &route_data, route_accounts.clone());
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_price(375_000),
            ComputeBudgetInstruction::set_compute_unit_limit(100_000),
        ];
        if !existing_wrapped {
            instructions.push(
                spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
                    &payer,
                    &wallet,
                    &native_mint,
                    &token_program,
                ),
            );
        }
        let jupiter_index = instructions.len();
        instructions.extend([
            route,
            spl_token_interface::instruction::close_account(
                &token_program,
                &source,
                &wallet,
                &wallet,
                &[],
            )
            .unwrap(),
            spl_token_interface::instruction::close_account(
                &token_program,
                &wrapped,
                &wallet,
                &wallet,
                &[],
            )
            .unwrap(),
            transfer(&wallet, &treasury, settlement),
        ]);
        let mut message = solana_message::v0::Message::try_compile(
            &payer,
            &instructions,
            &[],
            Hash::new_unique(),
        )
        .unwrap();
        message.address_table_lookups.push(solana_message::v0::MessageAddressTableLookup {
            account_key: lookup_table,
            writable_indexes: vec![],
            readonly_indexes: vec![],
        });
        let mut transaction = TransactionUtil::new_unsigned_versioned_transaction_resolved(
            VersionedMessage::V0(message),
        )
        .unwrap();
        if !existing_wrapped {
            let create = create_account(&payer, &wrapped, setup_rent, 165, &token_program);
            transaction.all_instructions.push(create.clone());
            transaction.inner_instruction_contexts.push(InnerInstructionContext {
                instruction: create,
                outer_instruction_index: 2,
                stack_height: Some(2),
            });
        }
        let mut raydium_data = if semantic_mutation == Some(10) {
            RAYDIUM_SWAP_V2_DISCRIMINATOR.to_vec()
        } else {
            RAYDIUM_SWAP_DISCRIMINATOR.to_vec()
        };
        raydium_data.resize(41, 0);
        let raydium = Instruction::new_with_bytes(raydium_program, &raydium_data, raydium_accounts);
        transaction.all_instructions.push(raydium.clone());
        transaction.inner_instruction_contexts.push(InnerInstructionContext {
            instruction: raydium,
            outer_instruction_index: jupiter_index as u8,
            stack_height: Some(2),
        });

        let mut policy = FeePayerPolicy::default();
        policy.system.recover = RecoverPolicy {
            enabled: true,
            route_policy: "semantic_family".to_string(),
            approved_dex_family: "RAYDIUM_CLMM".to_string(),
            allowed_users: vec![crate::config::RecoverUserPolicy {
                wallet: wallet.to_string(),
                source_account: source.to_string(),
                wrapped_sol_account: wrapped.to_string(),
            }],
            settlement_wallet: treasury.to_string(),
            input_mint: mint.to_string(),
            decimals: 6,
            swap_fee_bps: 30,
            rent_fee_bps: 300,
            slippage_bps: 50,
            compute_unit_limit: 100_000,
            compute_unit_price_micro_lamports: 375_000,
            catastrophe_output_lamports: 1_000_000,
            minimum_user_payout_lamports: 1_000_000,
            approved_pool_accounts: vec![],
            allowed_lookup_tables: vec![],
            allowed_jupiter_auxiliary_accounts: vec![Pubkey::find_program_address(
                &[b"__event_authority"],
                &jupiter_program,
            )
            .0
            .to_string()],
            authorization_public_key: Pubkey::new_unique().to_string(),
            authorization_network: "mainnet-beta".to_string(),
            authorization_max_lifetime_seconds: 90,
        };
        let config = ConfigMockBuilder::new()
            .with_price_source(PriceSource::Mock)
            .with_allowed_programs(vec![
                SYSTEM_PROGRAM_ID.to_string(),
                token_program.to_string(),
                jupiter_program.to_string(),
                raydium_program.to_string(),
                spl_associated_token_account_interface::program::id().to_string(),
                solana_compute_budget_interface::id().to_string(),
            ])
            .with_max_allowed_lamports(2_100_000)
            .with_fee_payer_policy(policy)
            .build();
        update_config(config).unwrap();

        let mut source_data = vec![0_u8; 165];
        source_data[0..32].copy_from_slice(mint.as_ref());
        source_data[32..64].copy_from_slice(wallet.as_ref());
        source_data[64..72].copy_from_slice(&input_amount.to_le_bytes());
        source_data[108] = 1;
        let mut source_owner = token_program;
        match source_mutation {
            Some(0) => source_data[64..72].copy_from_slice(&(input_amount - 1).to_le_bytes()),
            Some(1) => source_data[0..32].copy_from_slice(Pubkey::new_unique().as_ref()),
            Some(2) => source_data[32..64].copy_from_slice(Pubkey::new_unique().as_ref()),
            Some(3) => source_data[72..76].copy_from_slice(&1_u32.to_le_bytes()),
            Some(4) => source_data[108] = 2,
            Some(5) => source_data[109..113].copy_from_slice(&1_u32.to_le_bytes()),
            Some(6) => source_data[129..133].copy_from_slice(&1_u32.to_le_bytes()),
            Some(7) => source_owner = spl_token_2022_interface::id(),
            _ => {}
        }
        let source_json = Some(
            json!({ "data": [base64::engine::general_purpose::STANDARD.encode(source_data), "base64"], "executable": false, "lamports": source_rent, "owner": source_owner.to_string(), "rentEpoch": 0 }),
        );
        let mut mint_data = vec![0_u8; 82];
        mint_data[44] = if source_mutation == Some(8) { 5 } else { 6 };
        mint_data[45] = 1;
        let mint_json = Some(
            json!({ "data": [base64::engine::general_purpose::STANDARD.encode(mint_data), "base64"], "executable": false, "lamports": 1_461_600, "owner": token_program.to_string(), "rentEpoch": 0 }),
        );
        let wrapped_json = if existing_wrapped {
            let mut data = vec![0_u8; 165];
            data[0..32].copy_from_slice(native_mint.as_ref());
            data[32..64].copy_from_slice(wallet.as_ref());
            data[108] = 1;
            data[109..113].copy_from_slice(&1_u32.to_le_bytes());
            data[113..121].copy_from_slice(&setup_rent.to_le_bytes());
            let mut owner = token_program;
            match wrapped_mutation {
                Some(0) => data[64..72].copy_from_slice(&1_u64.to_le_bytes()),
                Some(1) => data[0..32].copy_from_slice(Pubkey::new_unique().as_ref()),
                Some(2) => data[32..64].copy_from_slice(Pubkey::new_unique().as_ref()),
                Some(3) => data[72..76].copy_from_slice(&1_u32.to_le_bytes()),
                Some(4) => data[108] = 2,
                Some(5) => data[109..113].copy_from_slice(&0_u32.to_le_bytes()),
                Some(6) => data[129..133].copy_from_slice(&1_u32.to_le_bytes()),
                Some(7) => owner = spl_token_2022_interface::id(),
                Some(8) => data[113..121].copy_from_slice(&(setup_rent - 1).to_le_bytes()),
                _ => {}
            }
            Some(
                json!({ "data": [base64::engine::general_purpose::STANDARD.encode(data), "base64"], "executable": false, "lamports": setup_rent, "owner": owner.to_string(), "rentEpoch": 0 }),
            )
        } else {
            None
        };
        let account_json = |data: Vec<u8>, owner: Pubkey| {
            Some(json!({
                "data": [base64::engine::general_purpose::STANDARD.encode(data), "base64"],
                "executable": false, "lamports": 1, "owner": owner.to_string(), "rentEpoch": 0
            }))
        };
        let mut amm_data = vec![0_u8; 117];
        amm_data[..8].copy_from_slice(&RAYDIUM_AMM_CONFIG_DISCRIMINATOR);
        amm_data[8] = config_bump;
        amm_data[9..11].copy_from_slice(&config_index.to_le_bytes());
        amm_data[51..53].copy_from_slice(&1_u16.to_le_bytes());
        let mut pool_data = vec![0_u8; 1544];
        pool_data[..8].copy_from_slice(&RAYDIUM_POOL_DISCRIMINATOR);
        pool_data[9..41].copy_from_slice(amm_config.as_ref());
        pool_data[73..105].copy_from_slice(native_mint.as_ref());
        pool_data[105..137].copy_from_slice(mint.as_ref());
        pool_data[137..169].copy_from_slice(output_vault.as_ref());
        pool_data[169..201].copy_from_slice(input_vault.as_ref());
        pool_data[201..233].copy_from_slice(observation.as_ref());
        pool_data[233] = 9;
        pool_data[234] = 6;
        pool_data[235..237].copy_from_slice(&1_u16.to_le_bytes());
        pool_data[269..273].copy_from_slice(&1_i32.to_le_bytes());
        if semantic_mutation == Some(11) {
            pool_data[269..273].copy_from_slice(&(-61_i32).to_le_bytes());
        }
        if semantic_mutation == Some(1) {
            pool_data[0] ^= 1;
        }
        if semantic_mutation == Some(6) {
            amm_data[9..11].copy_from_slice(&1_u16.to_le_bytes());
        }
        if semantic_mutation == Some(7) {
            amm_data[51..53].copy_from_slice(&2_u16.to_le_bytes());
        }
        if semantic_mutation == Some(8) {
            pool_data[389] |= 1 << 4;
        }
        let vault_json = |vault_mint: Pubkey, authority: Pubkey| {
            let mut data = vec![0_u8; 165];
            data[..32].copy_from_slice(vault_mint.as_ref());
            data[32..64].copy_from_slice(authority.as_ref());
            data[108] = 1;
            account_json(data, token_program)
        };
        let mut observation_data = vec![0_u8; 4483];
        observation_data[..8].copy_from_slice(&RAYDIUM_OBSERVATION_DISCRIMINATOR);
        observation_data[8] = 1;
        let observation_pool =
            if semantic_mutation == Some(2) { Pubkey::new_unique() } else { pool };
        observation_data[19..51].copy_from_slice(observation_pool.as_ref());
        let tick_json = |start: i32| {
            let mut data = vec![0_u8; 10240];
            data[..8].copy_from_slice(&RAYDIUM_TICK_ARRAY_DISCRIMINATOR);
            data[8..40].copy_from_slice(pool.as_ref());
            data[40..44].copy_from_slice(&start.to_le_bytes());
            account_json(data, raydium_program)
        };
        let mut bitmap_data = vec![0_u8; 1832];
        bitmap_data[..8].copy_from_slice(&RAYDIUM_BITMAP_DISCRIMINATOR);
        bitmap_data[8..40].copy_from_slice(pool.as_ref());
        let mut account_values = vec![
            source_json,
            mint_json,
            wrapped_json,
            account_json(
                amm_data,
                if semantic_mutation == Some(0) { token_program } else { raydium_program },
            ),
            account_json(pool_data, raydium_program),
            vault_json(
                mint,
                if semantic_mutation == Some(5) { Pubkey::new_unique() } else { pool },
            ),
            vault_json(native_mint, pool),
            account_json(observation_data, raydium_program),
            if semantic_mutation == Some(3) {
                let mut data = vec![0_u8; 10240];
                data[..8].copy_from_slice(&RAYDIUM_TICK_ARRAY_DISCRIMINATOR);
                data[8..40].copy_from_slice(pool.as_ref());
                account_json(data, token_program)
            } else {
                tick_json(0)
            },
            account_json(
                bitmap_data,
                if semantic_mutation == Some(4) { token_program } else { raydium_program },
            ),
            tick_json(60),
            tick_json(120),
        ];
        if semantic_mutation == Some(9) {
            account_values.remove(10);
        }
        let mut mocks = HashMap::new();
        mocks.insert(
            RpcRequest::GetMultipleAccounts,
            json!({ "context": { "slot": 1 }, "value": account_values }),
        );
        mocks.insert(
            RpcRequest::GetFeeForMessage,
            json!({ "context": { "slot": 1 }, "value": network_fee }),
        );
        mocks.insert(RpcRequest::GetMinimumBalanceForRentExemption, json!(setup_rent));
        let rpc = RpcMockBuilder::new().with_custom_mocks(mocks).build();
        (
            TransactionValidator::new(payer).unwrap(),
            transaction,
            rpc,
            if existing_wrapped { 0 } else { 1 },
        )
    }

    #[tokio::test]
    #[serial]
    async fn recover_accepts_only_missing_or_exact_safe_existing_wrapped_sol() {
        for existing in [false, true] {
            let (validator, transaction, rpc, payer_creations) =
                recover_fixture(existing, None, None);
            assert!(validator.validate_recover(&transaction, &rpc, payer_creations).await.is_ok());
        }
        for mutation in 0..=8 {
            let (validator, transaction, rpc, payer_creations) =
                recover_fixture(true, None, Some(mutation));
            assert!(
                validator.validate_recover(&transaction, &rpc, payer_creations).await.is_err(),
                "unsafe wrapped SOL state mutation {mutation} must fail"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn recover_accepts_independent_live_outputs_without_policy_changes() {
        for quoted_output in [1_000_000_000, 900_000_000] {
            let (validator, transaction, rpc, payer_creations) =
                recover_fixture_with_output(false, None, None, quoted_output);
            assert!(
                validator.validate_recover(&transaction, &rpc, payer_creations).await.is_ok(),
                "legitimate quote output {quoted_output} must pass the same policy"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn recover_accepts_historical_and_current_pool_identities_by_semantics() {
        for pool in [
            "3nMFwZXwY1s1M5s8vYAHqd4wGs4iSxXE4LRoUMMYqEgF",
            "FKzoAV4wZYteNV1xDDr5nQaSLrYEUWwerxvBkKVXNCB",
        ] {
            let (validator, transaction, rpc, payer_creations) =
                recover_fixture_with_output_and_semantic_mutation(
                    false,
                    None,
                    None,
                    1_000_000_000,
                    None,
                    Some(stable_recover_identity(
                        Pubkey::from_str(pool).unwrap(),
                        Pubkey::new_from_array([11; 32]),
                    )),
                );
            assert!(
                validator.validate_recover(&transaction, &rpc, payer_creations).await.is_ok(),
                "semantically valid pool {pool} must not require a config snapshot"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn recover_accepts_three_route_variants_under_one_stable_policy() {
        let variants = [
            (
                Pubkey::from_str("3nMFwZXwY1s1M5s8vYAHqd4wGs4iSxXE4LRoUMMYqEgF").unwrap(),
                Pubkey::new_from_array([11; 32]),
                1_000_000_000,
            ),
            (
                Pubkey::from_str("FKzoAV4wZYteNV1xDDr5nQaSLrYEUWwerxvBkKVXNCB").unwrap(),
                Pubkey::new_from_array([12; 32]),
                900_000_000,
            ),
            (Pubkey::new_from_array([21; 32]), Pubkey::new_from_array([13; 32]), 800_000_000),
        ];
        let mut stable_policy = None;
        for (pool, lookup_table, quoted_output) in variants {
            let (validator, transaction, rpc, payer_creations) =
                recover_fixture_with_output_and_semantic_mutation(
                    false,
                    None,
                    None,
                    quoted_output,
                    None,
                    Some(stable_recover_identity(pool, lookup_table)),
                );
            let policy = &validator.fee_payer_policy.system.recover;
            let identity = &policy.allowed_users[0];
            let snapshot = (
                identity.wallet.clone(),
                policy.input_mint.clone(),
                identity.source_account.clone(),
                identity.wrapped_sol_account.clone(),
                policy.settlement_wallet.clone(),
            );
            assert_eq!(stable_policy.get_or_insert_with(|| snapshot.clone()), &snapshot);
            assert!(validator.validate_recover(&transaction, &rpc, payer_creations).await.is_ok());
        }
    }

    #[tokio::test]
    #[serial]
    async fn recover_accepts_live_legacy_tick_gaps_and_exact_swap_v2_layout() {
        for semantic_variant in [9, 10] {
            let (validator, transaction, rpc, payer_creations) =
                recover_fixture_with_output_and_semantic_mutation(
                    false,
                    None,
                    None,
                    1_000_000_000,
                    Some(semantic_variant),
                    None,
                );
            assert!(
                validator.validate_recover(&transaction, &rpc, payer_creations).await.is_ok(),
                "valid Raydium route variant {semantic_variant} must pass"
            );
        }

        let (validator, mut transaction, rpc, payer_creations) =
            recover_fixture_with_output_and_semantic_mutation(
                false,
                None,
                None,
                1_000_000_000,
                Some(10),
                None,
            );
        transaction.inner_instruction_contexts.last_mut().unwrap().instruction.data[..8]
            .copy_from_slice(&RAYDIUM_SWAP_DISCRIMINATOR);
        assert!(validator.validate_recover(&transaction, &rpc, payer_creations).await.is_err());

        let (validator, transaction, rpc, payer_creations) =
            recover_fixture_with_output_and_semantic_mutation(
                false,
                None,
                None,
                1_000_000_000,
                Some(11),
                None,
            );
        assert!(validator.validate_recover(&transaction, &rpc, payer_creations).await.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn recover_exact_snapshot_preserves_pool_and_lut_pinning() {
        let (mut validator, transaction, rpc, payer_creations) = recover_fixture(false, None, None);
        let lookup_table = match &transaction.transaction.message {
            VersionedMessage::V0(message) => message.address_table_lookups[0].account_key,
            _ => panic!("Recover fixture must use a v0 message"),
        };
        let pool =
            transaction.inner_instruction_contexts.last().unwrap().instruction.accounts[2].pubkey;
        validator.fee_payer_policy.system.recover.route_policy = "exact_snapshot".to_string();
        validator.fee_payer_policy.system.recover.approved_pool_accounts = vec![pool.to_string()];
        validator.fee_payer_policy.system.recover.allowed_lookup_tables =
            vec![lookup_table.to_string()];
        assert!(validator.validate_recover(&transaction, &rpc, payer_creations).await.is_ok());

        let (mut validator, transaction, rpc, payer_creations) = recover_fixture(false, None, None);
        validator.fee_payer_policy.system.recover.route_policy = "exact_snapshot".to_string();
        validator.fee_payer_policy.system.recover.approved_pool_accounts =
            vec![Pubkey::new_unique().to_string()];
        validator.fee_payer_policy.system.recover.allowed_lookup_tables =
            vec![Pubkey::new_unique().to_string()];
        assert!(validator.validate_recover(&transaction, &rpc, payer_creations).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn recover_rejects_malformed_or_wrong_owner_raydium_state() {
        for mutation in 0..=8 {
            let (validator, transaction, rpc, payer_creations) =
                recover_fixture_with_output_and_semantic_mutation(
                    false,
                    None,
                    None,
                    1_000_000_000,
                    Some(mutation),
                    None,
                );
            assert!(
                validator.validate_recover(&transaction, &rpc, payer_creations).await.is_err(),
                "Raydium state mutation {mutation} must fail"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn recover_rejects_raydium_account_substitution_and_shape_mutations() {
        for mutation in 0..8 {
            let (validator, mut transaction, rpc, payer_creations) =
                recover_fixture(false, None, None);
            let raydium =
                &mut transaction.inner_instruction_contexts.last_mut().unwrap().instruction;
            match mutation {
                0 => raydium.accounts[2].pubkey = Pubkey::new_unique(),
                1 => raydium.accounts[5].pubkey = Pubkey::new_unique(),
                2 => raydium.accounts[9].pubkey = Pubkey::new_unique(),
                3 => raydium.accounts[9].is_writable = false,
                4 => raydium.accounts[10].is_signer = true,
                5 => raydium.accounts.push(AccountMeta::new(Pubkey::new_unique(), false)),
                6 => raydium.data[..8].copy_from_slice(&RAYDIUM_SWAP_V2_DISCRIMINATOR),
                _ => {
                    raydium.data.pop();
                }
            }
            assert!(
                validator.validate_recover(&transaction, &rpc, payer_creations).await.is_err(),
                "Raydium route mutation {mutation} must fail"
            );
        }

        let (validator, mut transaction, rpc, payer_creations) = recover_fixture(false, None, None);
        let arbitrary = Pubkey::new_unique();
        transaction.all_instructions[3].accounts.push(AccountMeta::new(arbitrary, false));
        transaction.all_account_keys.push(arbitrary);
        assert!(validator.validate_recover(&transaction, &rpc, payer_creations).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn recover_allows_an_unused_pinned_sponsor_meta_but_rejects_sponsor_cpi_use() {
        let (mut validator, mut transaction, rpc, payer_creations) =
            recover_fixture(false, None, None);
        let payer = validator.fee_payer_pubkey;
        validator
            .fee_payer_policy
            .system
            .recover
            .allowed_jupiter_auxiliary_accounts
            .push(payer.to_string());
        transaction.all_instructions[3].accounts.push(AccountMeta::new(payer, true));
        assert!(validator.validate_recover(&transaction, &rpc, payer_creations).await.is_ok());

        let sponsor_cpi = transfer(&payer, &Pubkey::new_unique(), 1);
        transaction.inner_instruction_contexts.push(InnerInstructionContext {
            instruction: sponsor_cpi,
            outer_instruction_index: 3,
            stack_height: Some(2),
        });
        assert!(validator.validate_recover(&transaction, &rpc, payer_creations).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn recover_authorization_economics_must_match_each_rpc_verified_value() {
        let (validator, mut transaction, rpc, payer_creations) =
            recover_fixture_with_output(false, None, None, 1_000_000_000);
        let minimum = 995_000_000_u64;
        let swap_fee = minimum * 30 / 10_000;
        let rent_fee = 2_039_280_u64 * 300 / 10_000;
        let network = 42_500_u64;
        let setup = 2_039_280_u64;
        let settlement = swap_fee + rent_fee + network + setup;
        let claims = RecoverAuthorizationClaims {
            schema_version: "recover-authorization-v1".to_string(),
            action: "CLEAN_RECOVER".to_string(),
            network: "mainnet-beta".to_string(),
            pilot_wallet: validator.fee_payer_policy.system.recover.allowed_users[0].wallet.clone(),
            source_token_account: validator.fee_payer_policy.system.recover.allowed_users[0]
                .source_account
                .clone(),
            input_mint: validator.fee_payer_policy.system.recover.input_mint.clone(),
            input_amount_raw: "1779926".to_string(),
            output_mint: "So11111111111111111111111111111111111111112".to_string(),
            expected_output_lamports: "1000000000".to_string(),
            minimum_output_lamports: minimum.to_string(),
            minimum_user_payout_lamports: (minimum + 2_039_280 + setup - settlement).to_string(),
            swap_fee_lamports: swap_fee.to_string(),
            rent_fee_lamports: rent_fee.to_string(),
            network_reimbursement_lamports: network.to_string(),
            setup_rent_reimbursement_lamports: setup.to_string(),
            sponsored_cost_lamports: (network + setup).to_string(),
            treasury: validator.fee_payer_policy.system.recover.settlement_wallet.clone(),
            message_hash: "verified-before-structural-policy".to_string(),
            quote_id: "quote".to_string(),
            intent_id: "intent".to_string(),
            nonce: "nonce".to_string(),
            issued_at_unix_seconds: 1,
            expires_at_unix_seconds: 2,
        };
        transaction.recover_authorization_claims = Some(claims.clone());
        assert!(validator.validate_recover(&transaction, &rpc, payer_creations).await.is_ok());
        for field in 0..9 {
            let mut mutated = transaction.clone();
            let claims = mutated.recover_authorization_claims.as_mut().unwrap();
            match field {
                0 => claims.input_amount_raw = "1779925".to_string(),
                1 => claims.expected_output_lamports = "999999999".to_string(),
                2 => claims.minimum_output_lamports = "994999999".to_string(),
                3 => claims.minimum_user_payout_lamports = "1".to_string(),
                4 => claims.swap_fee_lamports = (swap_fee + 1).to_string(),
                5 => claims.rent_fee_lamports = (rent_fee + 1).to_string(),
                6 => claims.network_reimbursement_lamports = (network + 1).to_string(),
                7 => claims.setup_rent_reimbursement_lamports = (setup + 1).to_string(),
                _ => claims.sponsored_cost_lamports = (network + setup + 1).to_string(),
            }
            assert!(
                validator.validate_recover(&mutated, &rpc, payer_creations).await.is_err(),
                "authorization economics mutation {field} must fail"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn recover_rejects_weakened_or_missing_jupiter_output_binding() {
        let (validator, original, rpc, payer_creations) = recover_fixture(false, None, None);
        for mutation in 0..4 {
            let mut transaction = original.clone();
            let route = &mut transaction.all_instructions[3];
            match mutation {
                0 => route.data[16..24].copy_from_slice(&900_000_000_u64.to_le_bytes()),
                1 => route.data[16..24].copy_from_slice(&0_u64.to_le_bytes()),
                2 => route.data.truncate(23),
                _ => route.data[0] ^= 1,
            }
            assert!(
                validator.validate_recover(&transaction, &rpc, payer_creations).await.is_err(),
                "minimum-output mutation {mutation} must fail"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn recover_rejects_source_state_and_route_settlement_mutations() {
        for mutation in 0..=8 {
            let (validator, transaction, rpc, payer_creations) =
                recover_fixture(false, Some(mutation), None);
            assert!(
                validator.validate_recover(&transaction, &rpc, payer_creations).await.is_err(),
                "source state mutation {mutation} must fail"
            );
        }
        let (validator, original, rpc, payer_creations) = recover_fixture(false, None, None);
        for mutation in 0..20 {
            let mut transaction = original.clone();
            match mutation {
                0 => transaction.all_instructions[0].data[1] ^= 1,
                1 => transaction.all_instructions[1].data[1] ^= 1,
                2 => transaction.all_instructions.swap(0, 1),
                3 => transaction.all_instructions[3].data[8] ^= 1,
                4 => transaction.all_instructions[3].data[24] = 51,
                5 => transaction.all_instructions[3].data[26] = 1,
                6 => transaction.all_instructions[3].program_id = Pubkey::new_unique(),
                7 => transaction.all_instructions[3].accounts[0].pubkey = Pubkey::new_unique(),
                8 => transaction.all_instructions[4].accounts[0].pubkey = Pubkey::new_unique(),
                9 => transaction.all_instructions[4].accounts[1].pubkey = Pubkey::new_unique(),
                10 => transaction.all_instructions[5].accounts[0].pubkey = Pubkey::new_unique(),
                11 => transaction.all_instructions[5].accounts[1].pubkey = Pubkey::new_unique(),
                12 => transaction.all_instructions[6].accounts[1].pubkey = Pubkey::new_unique(),
                13 => {
                    transaction.all_instructions[6].data =
                        bincode::serialize(&SystemInstruction::Transfer { lamports: 1 }).unwrap()
                }
                14 => transaction
                    .all_instructions
                    .insert(3, transfer(&validator.fee_payer_pubkey, &Pubkey::new_unique(), 1)),
                15 => transaction.all_instructions[4].data[0] = 4,
                16 => {
                    transaction
                        .inner_instruction_contexts
                        .last_mut()
                        .unwrap()
                        .instruction
                        .program_id = Pubkey::new_unique()
                }
                17 => match &mut transaction.transaction.message {
                    VersionedMessage::V0(message) => {
                        message.address_table_lookups[0].account_key = Pubkey::new_unique()
                    }
                    _ => unreachable!(),
                },
                18 => match &mut transaction.transaction.message {
                    VersionedMessage::V0(message) => {
                        message.header.num_required_signatures = 3;
                        message.account_keys.insert(2, Pubkey::new_unique());
                    }
                    _ => unreachable!(),
                },
                _ => transaction.all_instructions.push(Instruction::new_with_bytes(
                    Pubkey::new_unique(),
                    &[],
                    vec![],
                )),
            }
            assert!(
                validator.validate_recover(&transaction, &rpc, payer_creations).await.is_err(),
                "Recover adversarial mutation {mutation} must fail"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn clean_claim_and_burn_accept_only_exact_opt_in_shapes() {
        for (burn, claim_accounts) in [(false, 1), (false, 2), (false, 10), (true, 1)] {
            let (validator, transaction, rpc, _, _) =
                clean_fixture(burn, !burn, burn, claim_accounts);
            assert!(validator.validate_clean(&transaction, &rpc).await.is_ok());
        }
    }

    #[tokio::test]
    #[serial]
    async fn clean_claim_rejects_over_limit_and_disabled_cross_feature_shapes() {
        let (validator, transaction, rpc, _, _) = clean_fixture(false, true, false, 11);
        assert!(validator.validate_clean(&transaction, &rpc).await.is_err());

        let (validator, transaction, rpc, _, _) = clean_fixture(false, false, true, 1);
        assert!(validator.validate_clean(&transaction, &rpc).await.is_err());

        let (validator, transaction, rpc, _, _) = clean_fixture(true, true, false, 1);
        assert!(validator.validate_clean(&transaction, &rpc).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn clean_claim_rejects_ineligible_current_account_state() {
        for mutation in 0..8 {
            let (validator, transaction, rpc, _, _) =
                clean_fixture_with_account_mutation(false, true, false, 1, Some(mutation));
            assert!(
                validator.validate_clean(&transaction, &rpc).await.is_err(),
                "account mutation {mutation} must fail"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn clean_policy_rejects_compute_settlement_account_and_instruction_mutations() {
        let (validator, original, rpc, _, _) = clean_fixture(false, true, false, 1);
        for mutation in 0..12 {
            let mut transaction = original.clone();
            match mutation {
                0 => transaction.all_instructions[0].data[1] ^= 1,
                1 => transaction.all_instructions[1].data[1] ^= 1,
                2 => transaction.all_instructions.swap(0, 1),
                3 => {
                    transaction.all_instructions.last_mut().unwrap().accounts[1].pubkey =
                        Pubkey::new_unique()
                }
                4 => {
                    transaction.all_instructions.last_mut().unwrap().data =
                        bincode::serialize(&SystemInstruction::Transfer { lamports: 103_677 })
                            .unwrap()
                }
                5 => {
                    transaction.all_instructions.insert(2, transaction.all_instructions[2].clone())
                }
                6 => transaction.all_instructions[2].accounts[1].pubkey = Pubkey::new_unique(),
                7 => transaction.all_instructions[2].accounts[2].pubkey = Pubkey::new_unique(),
                8 => {
                    transaction.all_instructions.last_mut().unwrap().accounts[0].pubkey =
                        validator.fee_payer_pubkey
                }
                9 => transaction.all_instructions.insert(
                    transaction.all_instructions.len() - 1,
                    transaction.all_instructions.last().unwrap().clone(),
                ),
                10 => match &mut transaction.transaction.message {
                    VersionedMessage::V0(message) => {
                        message.account_keys.push(Pubkey::new_unique())
                    }
                    _ => unreachable!(),
                },
                _ => match &mut transaction.transaction.message {
                    VersionedMessage::V0(message) => message.address_table_lookups.push(
                        solana_message::v0::MessageAddressTableLookup {
                            account_key: Pubkey::new_unique(),
                            writable_indexes: vec![],
                            readonly_indexes: vec![],
                        },
                    ),
                    _ => unreachable!(),
                },
            }
            assert!(
                validator.validate_clean(&transaction, &rpc).await.is_err(),
                "mutation {mutation} must fail"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn clean_burn_rejects_full_hosted_adversarial_matrix() {
        let (validator, original, rpc, wallet, treasury) = clean_fixture(true, false, true, 1);
        assert!(validator.validate_clean(&original, &rpc).await.is_ok());
        for mutation in 0..23 {
            let mut transaction = original.clone();
            match mutation {
                0 => transaction.all_instructions[2].data[1] -= 1, // partial burn
                1 => transaction.all_instructions[2].data[1] += 1, // amount +1
                2 => transaction.all_instructions[2].accounts[0].pubkey = Pubkey::new_unique(),
                3 => transaction.all_instructions[2].accounts[1].pubkey = Pubkey::new_unique(),
                4 => transaction.all_instructions[2].accounts[2].pubkey = Pubkey::new_unique(),
                5 => transaction.all_instructions[3].accounts[1].pubkey = Pubkey::new_unique(),
                6 => transaction.all_instructions[3].data.clear(),
                7 => {
                    transaction.all_instructions.insert(3, transaction.all_instructions[3].clone());
                }
                8 => transaction.all_instructions[4].accounts[1].pubkey = Pubkey::new_unique(),
                9 => {
                    transaction.all_instructions[4].data =
                        bincode::serialize(&SystemInstruction::Transfer { lamports: 1 }).unwrap()
                }
                10 => {
                    transaction.all_instructions[4].accounts[0].pubkey = validator.fee_payer_pubkey
                }
                11 => transaction.all_instructions[0].data[1] ^= 1,
                12 => transaction.all_instructions[1].data[1] ^= 1,
                13 => transaction.all_instructions.swap(0, 1),
                14 => match &mut transaction.transaction.message {
                    VersionedMessage::V0(message) => {
                        message.header.num_required_signatures = 3;
                        message.account_keys.insert(2, Pubkey::new_unique());
                    }
                    _ => unreachable!(),
                },
                15 => transaction.all_instructions[2].program_id = spl_token_2022_interface::id(),
                16 => transaction.all_instructions[2].data[0] = 3, // Transfer
                17 => transaction.all_instructions[2].data[0] = 4, // Approve
                18 => transaction.all_instructions[2].data[0] = 6, // SetAuthority
                19 => transaction.all_instructions[2].data[0] = 7, // MintTo
                20 => transaction.all_instructions[2].program_id = Pubkey::new_unique(),
                21 => match &mut transaction.transaction.message {
                    VersionedMessage::V0(message) => message.address_table_lookups.push(
                        solana_message::v0::MessageAddressTableLookup {
                            account_key: Pubkey::new_unique(),
                            writable_indexes: vec![],
                            readonly_indexes: vec![],
                        },
                    ),
                    _ => unreachable!(),
                },
                _ => transaction.all_instructions[2].data[9] ^= 1, // BurnChecked decimals
            }
            assert!(
                validator.validate_clean(&transaction, &rpc).await.is_err(),
                "Burn adversarial mutation {mutation} must fail"
            );
        }
        assert_ne!(wallet, validator.fee_payer_pubkey);
        assert_ne!(treasury, validator.fee_payer_pubkey);
    }

    #[tokio::test]
    #[serial]
    async fn gasless_canonical_ata_exception_accepts_only_exact_direct_shape() {
        let (validator, transaction, rpc, _, _, _) = canonical_ata_fixture(2_039_280, false);
        assert!(validator.validate_canonical_ata_creation(&transaction, &rpc, 1).await.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn gasless_canonical_ata_exception_rejects_mutated_fields_and_existing_destination() {
        let (validator, original, rpc, _, _, _) = canonical_ata_fixture(2_039_280, false);
        for mutation in 0..7 {
            let mut transaction = original.clone();
            match mutation {
                0 => transaction.all_instructions[0].accounts[1].pubkey = Pubkey::new_unique(),
                1 => transaction.all_instructions[0].accounts[2].pubkey = Pubkey::new_unique(),
                2 => transaction.all_instructions[0].accounts[3].pubkey = Pubkey::new_unique(),
                3 => {
                    transaction.all_instructions[0].accounts[5].pubkey =
                        spl_token_2022_interface::id()
                }
                4 => {
                    transaction.inner_instruction_contexts[0].instruction.data =
                        bincode::serialize(&SystemInstruction::CreateAccount {
                            lamports: 2_039_280,
                            space: 165,
                            owner: Pubkey::new_unique(),
                        })
                        .unwrap()
                }
                5 => {
                    transaction.inner_instruction_contexts[0].instruction.data =
                        bincode::serialize(&SystemInstruction::CreateAccount {
                            lamports: 2_039_279,
                            space: 165,
                            owner: spl_token_interface::id(),
                        })
                        .unwrap()
                }
                _ => {
                    transaction.inner_instruction_contexts[0].instruction.data =
                        bincode::serialize(&SystemInstruction::CreateAccount {
                            lamports: 2_039_280,
                            space: 164,
                            owner: spl_token_interface::id(),
                        })
                        .unwrap()
                }
            }
            assert!(
                validator.validate_canonical_ata_creation(&transaction, &rpc, 1).await.is_err(),
                "mutation {mutation} must fail"
            );
        }
        let (validator, transaction, rpc, _, _, _) = canonical_ata_fixture(2_039_280, true);
        assert!(validator.validate_canonical_ata_creation(&transaction, &rpc, 1).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn gasless_canonical_ata_exception_rejects_wrong_parent_depth_direct_seed_and_duplicates()
    {
        let (validator, original, rpc, payer, _, _) = canonical_ata_fixture(2_039_280, false);
        let mut wrong_parent = original.clone();
        wrong_parent.inner_instruction_contexts[0].outer_instruction_index = 1;
        assert!(validator.validate_canonical_ata_creation(&wrong_parent, &rpc, 1).await.is_err());
        let mut nested = original.clone();
        nested.inner_instruction_contexts[0].stack_height = Some(3);
        assert!(validator.validate_canonical_ata_creation(&nested, &rpc, 1).await.is_err());
        let mut direct = original.clone();
        direct.inner_instruction_contexts.clear();
        assert!(validator.validate_canonical_ata_creation(&direct, &rpc, 1).await.is_err());
        let mut seeded = original.clone();
        let destination = seeded.inner_instruction_contexts[0].instruction.accounts[1].pubkey;
        seeded.inner_instruction_contexts[0].instruction = create_account_with_seed(
            &payer,
            &destination,
            &payer,
            "seed",
            2_039_280,
            165,
            &spl_token_interface::id(),
        );
        assert!(validator.validate_canonical_ata_creation(&seeded, &rpc, 1).await.is_err());
        let mut duplicate = original.clone();
        duplicate.inner_instruction_contexts.push(duplicate.inner_instruction_contexts[0].clone());
        assert!(validator.validate_canonical_ata_creation(&duplicate, &rpc, 2).await.is_err());

        let mut duplicate_idempotent_noop = original.clone();
        let duplicate_ata = duplicate_idempotent_noop.all_instructions[0].clone();
        if let VersionedMessage::Legacy(message) =
            &mut duplicate_idempotent_noop.transaction.message
        {
            let duplicate_compiled_ata = message.instructions[0].clone();
            message.instructions.insert(1, duplicate_compiled_ata);
        } else {
            unreachable!("canonical ATA fixture uses a legacy message");
        }
        duplicate_idempotent_noop.all_instructions.insert(1, duplicate_ata);
        duplicate_idempotent_noop.inner_instruction_contexts[1].outer_instruction_index = 2;
        assert!(validator
            .validate_canonical_ata_creation(&duplicate_idempotent_noop, &rpc, 1)
            .await
            .is_err());

        let mut missing_raydium = original.clone();
        missing_raydium.inner_instruction_contexts.pop();
        missing_raydium.all_instructions.pop();
        assert!(validator
            .validate_canonical_ata_creation(&missing_raydium, &rpc, 1)
            .await
            .is_err());

        let mut raydium_wrong_parent = original.clone();
        raydium_wrong_parent.inner_instruction_contexts[1].outer_instruction_index = 0;
        assert!(validator
            .validate_canonical_ata_creation(&raydium_wrong_parent, &rpc, 1)
            .await
            .is_err());
    }

    fn send_token_account_json(
        mint: &Pubkey,
        owner: &Pubkey,
        amount: u64,
        token_program: &Pubkey,
        state: u8,
        delegated: bool,
    ) -> serde_json::Value {
        let mut data = vec![0_u8; 165];
        data[0..32].copy_from_slice(mint.as_ref());
        data[32..64].copy_from_slice(owner.as_ref());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        if delegated {
            data[72..76].copy_from_slice(&1_u32.to_le_bytes());
        }
        data[108] = state;
        json!({
            "data": [base64::engine::general_purpose::STANDARD.encode(data), "base64"],
            "executable": false,
            "lamports": 2_039_280,
            "owner": token_program.to_string(),
            "rentEpoch": 0
        })
    }

    fn send_ata_fixture_with_source(
        existing: bool,
        source_state: u8,
        source_amount: u64,
        source_delegated: bool,
    ) -> (
        TransactionValidator,
        VersionedTransactionResolved,
        std::sync::Arc<RpcClient>,
        Pubkey,
        Pubkey,
        Pubkey,
        Pubkey,
        Pubkey,
    ) {
        let payer = Pubkey::new_unique();
        let sender = Pubkey::new_unique();
        let recipient = loop {
            let candidate = Pubkey::new_unique();
            if candidate.is_on_curve() {
                break candidate;
            }
        };
        let treasury = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let token_program = spl_token_interface::id();
        let source = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&sender, &mint, &token_program);
        let destination = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&recipient, &mint, &token_program);
        let settlement = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&treasury, &mint, &token_program);
        let mut policy = FeePayerPolicy::default();
        policy.system.send = SendPolicy {
            enabled: true,
            settlement_wallet: treasury.to_string(),
            approved_mints: vec![SendMintPolicy { mint: mint.to_string(), decimals: 6 }],
        };
        setup_config_with_policy(policy);
        let ata = spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(&payer, &recipient, &mint, &token_program);
        let recipient_transfer = spl_token_interface::instruction::transfer_checked(
            &token_program,
            &source,
            &mint,
            &destination,
            &sender,
            &[],
            500_000,
            6,
        )
        .unwrap();
        let reimbursement = spl_token_interface::instruction::transfer_checked(
            &token_program,
            &source,
            &mint,
            &settlement,
            &sender,
            &[],
            2_100,
            6,
        )
        .unwrap();
        let service_fee = spl_token_interface::instruction::transfer_checked(
            &token_program,
            &source,
            &mint,
            &settlement,
            &sender,
            &[],
            500,
            6,
        )
        .unwrap();
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_price(SEND_COMPUTE_UNIT_PRICE_MICROLAMPORTS),
            ComputeBudgetInstruction::set_compute_unit_limit(SEND_COMPUTE_UNIT_LIMIT),
            recipient_transfer,
            reimbursement,
            service_fee,
        ];
        if !existing {
            instructions.insert(2, ata);
        }
        let message = VersionedMessage::Legacy(Message::new(&instructions, Some(&payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        if !existing {
            let create = create_account(&payer, &destination, 2_039_280, 165, &token_program);
            transaction.all_instructions.push(create.clone());
            transaction.inner_instruction_contexts.push(InnerInstructionContext {
                instruction: create,
                outer_instruction_index: 2,
                stack_height: Some(2),
            });
        }
        let mut mocks = HashMap::new();
        mocks.insert(RpcRequest::GetMinimumBalanceForRentExemption, json!(2_039_280));
        mocks.insert(
            RpcRequest::GetMultipleAccounts,
            json!({
                "context": { "slot": 1 },
                "value": [
                    if existing { Some(send_token_account_json(&mint, &recipient, 0, &token_program, 1, false)) } else { None },
                    Some(send_token_account_json(&mint, &sender, source_amount, &token_program, source_state, source_delegated)),
                    Some(send_token_account_json(&mint, &treasury, 0, &token_program, 1, false))
                ]
            }),
        );
        let rpc = RpcMockBuilder::new().with_custom_mocks(mocks).build();
        (
            TransactionValidator::new(payer).unwrap(),
            transaction,
            rpc,
            payer,
            sender,
            recipient,
            mint,
            settlement,
        )
    }

    fn send_ata_fixture(
        existing: bool,
    ) -> (
        TransactionValidator,
        VersionedTransactionResolved,
        std::sync::Arc<RpcClient>,
        Pubkey,
        Pubkey,
        Pubkey,
        Pubkey,
        Pubkey,
    ) {
        send_ata_fixture_with_source(existing, 1, 1_000_000, false)
    }

    #[tokio::test]
    #[serial]
    async fn gasless_send_ata_exception_accepts_only_exact_standalone_shape() {
        let (validator, transaction, rpc, _, _, _, _, _) = send_ata_fixture(false);
        let result = validator.validate_send(&transaction, &rpc, 1).await;
        assert!(result.is_ok(), "{result:?}");

        let mut wrong_price = transaction.clone();
        wrong_price.all_instructions[0].data = ComputeBudgetInstruction::set_compute_unit_price(
            SEND_COMPUTE_UNIT_PRICE_MICROLAMPORTS + 1,
        )
        .data;
        assert!(validator.validate_send(&wrong_price, &rpc, 1).await.is_err());

        let mut wrong_limit = transaction.clone();
        wrong_limit.all_instructions[1].data =
            ComputeBudgetInstruction::set_compute_unit_limit(SEND_COMPUTE_UNIT_LIMIT + 1).data;
        assert!(validator.validate_send(&wrong_limit, &rpc, 1).await.is_err());

        let mut wrong_order = transaction.clone();
        wrong_order.all_instructions.swap(0, 1);
        assert!(validator.validate_send(&wrong_order, &rpc, 1).await.is_err());

        let mut missing_price = transaction.clone();
        missing_price.all_instructions.remove(0);
        assert!(validator.validate_send(&missing_price, &rpc, 1).await.is_err());

        let mut missing_limit = transaction.clone();
        missing_limit.all_instructions.remove(1);
        assert!(validator.validate_send(&missing_limit, &rpc, 1).await.is_err());

        let mut duplicate_price = transaction.clone();
        duplicate_price.all_instructions.insert(1, duplicate_price.all_instructions[0].clone());
        assert!(validator.validate_send(&duplicate_price, &rpc, 1).await.is_err());

        let mut duplicate_limit = transaction.clone();
        duplicate_limit.all_instructions.insert(2, duplicate_limit.all_instructions[1].clone());
        assert!(validator.validate_send(&duplicate_limit, &rpc, 1).await.is_err());

        let mut extra_compute = transaction.clone();
        extra_compute
            .all_instructions
            .insert(2, ComputeBudgetInstruction::set_compute_unit_limit(SEND_COMPUTE_UNIT_LIMIT));
        assert!(validator.validate_send(&extra_compute, &rpc, 1).await.is_err());

        let (validator, transaction, rpc, _, _, _, _, _) = send_ata_fixture(true);
        let result = validator.validate_send(&transaction, &rpc, 0).await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    #[serial]
    async fn gasless_send_policy_uses_runtime_mints_and_transaction_recipients() {
        let (validator, transaction, rpc, payer, _, recipient_a, mint_b, _) =
            send_ata_fixture(false);
        let treasury = validator.fee_payer_policy.system.send.settlement_wallet.clone();
        let unapproved_mint_c = Pubkey::new_unique();

        let mut policy = FeePayerPolicy::default();
        policy.system.send = SendPolicy {
            enabled: true,
            settlement_wallet: treasury.clone(),
            approved_mints: vec![SendMintPolicy {
                mint: unapproved_mint_c.to_string(),
                decimals: 6,
            }],
        };
        setup_config_with_policy(policy);
        let validator = TransactionValidator::new(payer).unwrap();
        assert!(validator.validate_send(&transaction, &rpc, 1).await.is_err());

        let mut policy = FeePayerPolicy::default();
        policy.system.send = SendPolicy {
            enabled: true,
            settlement_wallet: treasury,
            approved_mints: vec![SendMintPolicy { mint: mint_b.to_string(), decimals: 6 }],
        };
        setup_config_with_policy(policy);
        let validator = TransactionValidator::new(payer).unwrap();
        assert!(validator.validate_send(&transaction, &rpc, 1).await.is_ok());

        let (_, _, _, _, _, recipient_b, _, _) = send_ata_fixture(false);
        assert_ne!(recipient_a, recipient_b);
    }

    #[tokio::test]
    #[serial]
    async fn gasless_send_policy_rejects_reserved_and_off_curve_recipients() {
        let (validator, original, rpc, payer, sender, _, _, _) = send_ata_fixture(false);
        let treasury =
            Pubkey::from_str(&validator.fee_payer_policy.system.send.settlement_wallet).unwrap();
        let off_curve = loop {
            let candidate = Pubkey::new_unique();
            if !candidate.is_on_curve() {
                break candidate;
            }
        };
        for recipient in [payer, sender, treasury, off_curve] {
            let mut transaction = original.clone();
            transaction.all_instructions[2].accounts[2].pubkey = recipient;
            assert!(validator.validate_send(&transaction, &rpc, 1).await.is_err());
        }
    }

    #[tokio::test]
    #[serial]
    async fn gasless_send_ata_exception_rejects_identity_account_and_transfer_mutations() {
        let (validator, original, rpc, _, _, _, _, _) = send_ata_fixture(false);
        for mutation in 0..12 {
            let mut transaction = original.clone();
            match mutation {
                0 => transaction.all_instructions[2].accounts[0].pubkey = Pubkey::new_unique(),
                1 => transaction.all_instructions[2].accounts[1].pubkey = Pubkey::new_unique(),
                2 => transaction.all_instructions[2].accounts[2].pubkey = Pubkey::new_unique(),
                3 => transaction.all_instructions[2].accounts[3].pubkey = Pubkey::new_unique(),
                4 => {
                    transaction.all_instructions[2].accounts[5].pubkey =
                        spl_token_2022_interface::id()
                }
                5 => transaction.all_instructions[3].accounts[0].pubkey = Pubkey::new_unique(),
                6 => transaction.all_instructions[3].accounts[1].pubkey = Pubkey::new_unique(),
                7 => transaction.all_instructions[3].accounts[2].pubkey = Pubkey::new_unique(),
                8 => transaction.all_instructions[3].accounts[3].pubkey = Pubkey::new_unique(),
                9 => transaction.all_instructions[4].accounts[2].pubkey = Pubkey::new_unique(),
                10 => transaction.all_instructions[5].accounts[2].pubkey = Pubkey::new_unique(),
                _ => transaction.all_instructions[3].data[9] = 9,
            }
            assert!(
                validator.validate_send(&transaction, &rpc, 1).await.is_err(),
                "mutation {mutation} must fail"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn gasless_send_ata_exception_rejects_creation_shape_swap_and_existing_destination() {
        let (validator, original, rpc, payer, _, _, _, _) = send_ata_fixture(false);
        for mutation in 0..8 {
            let mut transaction = original.clone();
            match mutation {
                0 => transaction.inner_instruction_contexts[0].stack_height = Some(3),
                1 => transaction.inner_instruction_contexts[0].outer_instruction_index = 1,
                2 => {
                    transaction.inner_instruction_contexts[0].instruction = create_account_with_seed(
                        &payer,
                        &transaction.inner_instruction_contexts[0].instruction.accounts[1].pubkey,
                        &payer,
                        "seed",
                        2_039_280,
                        165,
                        &spl_token_interface::id(),
                    )
                }
                3 => {
                    transaction.inner_instruction_contexts[0].instruction.data =
                        bincode::serialize(&SystemInstruction::CreateAccount {
                            lamports: 2_039_279,
                            space: 165,
                            owner: spl_token_interface::id(),
                        })
                        .unwrap()
                }
                4 => {
                    transaction.inner_instruction_contexts[0].instruction.data =
                        bincode::serialize(&SystemInstruction::CreateAccount {
                            lamports: 2_039_280,
                            space: 164,
                            owner: spl_token_interface::id(),
                        })
                        .unwrap()
                }
                5 => {
                    transaction.inner_instruction_contexts[0].instruction.data =
                        bincode::serialize(&SystemInstruction::CreateAccount {
                            lamports: 2_039_280,
                            space: 165,
                            owner: Pubkey::new_unique(),
                        })
                        .unwrap()
                }
                6 => transaction
                    .inner_instruction_contexts
                    .push(transaction.inner_instruction_contexts[0].clone()),
                _ => {
                    transaction.all_instructions[3].program_id =
                        Pubkey::from_str(JUPITER_V6_PROGRAM_ID).unwrap()
                }
            }
            assert!(
                validator.validate_send(&transaction, &rpc, 1).await.is_err(),
                "mutation {mutation} must fail"
            );
        }
        let (validator, transaction, rpc, _, _, _, _, _) = send_ata_fixture(true);
        assert!(validator.validate_send(&transaction, &rpc, 1).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn gasless_send_ata_exception_rejects_extra_or_non_checked_transfers_and_signers() {
        let (validator, original, rpc, _, sender, _, _, _) = send_ata_fixture(false);
        let mut zero = original.clone();
        zero.all_instructions[3].data[1..9].copy_from_slice(&0_u64.to_le_bytes());
        assert!(validator.validate_send(&zero, &rpc, 1).await.is_err());

        let mut unchecked = original.clone();
        unchecked.all_instructions[3] = spl_token_interface::instruction::transfer(
            &spl_token_interface::id(),
            &unchecked.all_instructions[3].accounts[0].pubkey,
            &unchecked.all_instructions[3].accounts[2].pubkey,
            &sender,
            &[],
            500_000,
        )
        .unwrap();
        assert!(validator.validate_send(&unchecked, &rpc, 1).await.is_err());

        let mut inner_transfer = original.clone();
        inner_transfer.inner_instruction_contexts.push(InnerInstructionContext {
            instruction: inner_transfer.all_instructions[3].clone(),
            outer_instruction_index: 3,
            stack_height: Some(2),
        });
        assert!(validator.validate_send(&inner_transfer, &rpc, 1).await.is_err());

        let mut extra_signer = original.clone();
        if let VersionedMessage::Legacy(message) = &mut extra_signer.transaction.message {
            message.header.num_required_signatures = 3;
        }
        assert!(validator.validate_send(&extra_signer, &rpc, 1).await.is_err());

        for system_instruction in [
            transfer(&original.all_instructions[2].accounts[0].pubkey, &Pubkey::new_unique(), 1),
            solana_system_interface::instruction::allocate(
                &original.all_instructions[2].accounts[0].pubkey,
                1,
            ),
            assign(&original.all_instructions[2].accounts[0].pubkey, &Pubkey::new_unique()),
        ] {
            let mut transaction = original.clone();
            transaction.inner_instruction_contexts.push(InnerInstructionContext {
                instruction: system_instruction,
                outer_instruction_index: 2,
                stack_height: Some(2),
            });
            assert!(validator.validate_send(&transaction, &rpc, 1).await.is_err());
        }
    }

    #[tokio::test]
    #[serial]
    async fn gasless_send_ata_exception_rejects_unhealthy_or_underfunded_source() {
        for (state, amount, delegated) in
            [(2, 1_000_000, false), (1, 1_000_000, true), (1, 502_599, false)]
        {
            let (validator, transaction, rpc, _, _, _, _, _) =
                send_ata_fixture_with_source(false, state, amount, delegated);
            assert!(validator.validate_send(&transaction, &rpc, 1).await.is_err());
        }
    }

    fn setup_spl_config_with_policy(policy: FeePayerPolicy) {
        let config = ConfigMockBuilder::new()
            .with_price_source(PriceSource::Mock)
            .with_allowed_programs(vec![spl_token_interface::id().to_string()])
            .with_max_allowed_lamports(1_000_000)
            .with_fee_payer_policy(policy)
            .build();
        update_config(config).unwrap();
    }

    fn setup_token2022_config_with_policy(policy: FeePayerPolicy) {
        let config = ConfigMockBuilder::new()
            .with_price_source(PriceSource::Mock)
            .with_allowed_programs(vec![spl_token_2022_interface::id().to_string()])
            .with_max_allowed_lamports(1_000_000)
            .with_fee_payer_policy(policy)
            .build();
        let _guard = mock_state::setup_config_mock(config.clone());
        update_config(config).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_validate_transaction() {
        let fee_payer = Pubkey::new_unique();
        setup_default_config();
        let rpc_client = RpcMockBuilder::new().build();

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let recipient = Pubkey::new_unique();
        let sender = Pubkey::new_unique();
        let instruction = transfer(&sender, &recipient, 100_000);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn test_transfer_amount_limits() {
        let fee_payer = Pubkey::new_unique();
        setup_default_config();
        let rpc_client = RpcMockBuilder::new().build();

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let sender = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();

        // Test transaction with amount over limit
        let instruction = transfer(&sender, &recipient, 2_000_000);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test multiple transfers
        let instructions =
            vec![transfer(&sender, &recipient, 500_000), transfer(&sender, &recipient, 500_000)];
        let message = VersionedMessage::Legacy(Message::new(&instructions, Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn test_validate_programs() {
        let fee_payer = Pubkey::new_unique();
        setup_default_config();
        let rpc_client = RpcMockBuilder::new().build();

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let sender = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();

        // Test allowed program (system program)
        let instruction = transfer(&sender, &recipient, 1000);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test disallowed program
        let fake_program = Pubkey::new_unique();
        // Create a no-op instruction for the fake program
        let instruction = Instruction::new_with_bincode(
            fake_program,
            &[0u8],
            vec![], // no accounts needed for this test
        );
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_validate_signatures() {
        let fee_payer = Pubkey::new_unique();
        let config = ConfigMockBuilder::new()
            .with_price_source(PriceSource::Mock)
            .with_allowed_programs(vec![SYSTEM_PROGRAM_ID.to_string()])
            .with_max_allowed_lamports(1_000_000)
            .with_max_signatures(2)
            .with_fee_payer_policy(FeePayerPolicy::default())
            .build();
        update_config(config).unwrap();

        let rpc_client = RpcMockBuilder::new().build();
        let validator = TransactionValidator::new(fee_payer).unwrap();
        let sender = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();

        // Test too many signatures
        let instructions = vec![
            transfer(&sender, &recipient, 1000),
            transfer(&sender, &recipient, 1000),
            transfer(&sender, &recipient, 1000),
        ];
        let message = VersionedMessage::Legacy(Message::new(&instructions, Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        transaction.transaction.signatures = vec![Default::default(); 3]; // Add 3 dummy signatures
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_sign_and_send_transaction_mode() {
        let fee_payer = Pubkey::new_unique();
        setup_default_config();
        let rpc_client = RpcMockBuilder::new().build();

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let sender = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();

        // Test SignAndSend mode with fee payer already set should not error
        let instruction = transfer(&sender, &recipient, 1000);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test SignAndSend mode without fee payer (should succeed)
        let instruction = transfer(&sender, &recipient, 1000);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], None)); // No fee payer specified
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn test_empty_transaction() {
        let fee_payer = Pubkey::new_unique();
        setup_default_config();
        let rpc_client = RpcMockBuilder::new().build();

        let validator = TransactionValidator::new(fee_payer).unwrap();

        // Create an empty message using Message::new with empty instructions
        let message = VersionedMessage::Legacy(Message::new(&[], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_disallowed_accounts() {
        let fee_payer = Pubkey::new_unique();
        let config = ConfigMockBuilder::new()
            .with_price_source(PriceSource::Mock)
            .with_allowed_programs(vec![SYSTEM_PROGRAM_ID.to_string()])
            .with_max_allowed_lamports(1_000_000)
            .with_disallowed_accounts(vec![
                "hndXZGK45hCxfBYvxejAXzCfCujoqkNf7rk4sTB8pek".to_string()
            ])
            .with_fee_payer_policy(FeePayerPolicy::default())
            .build();
        update_config(config).unwrap();

        let rpc_client = RpcMockBuilder::new().build();
        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = transfer(
            &Pubkey::from_str("hndXZGK45hCxfBYvxejAXzCfCujoqkNf7rk4sTB8pek").unwrap(),
            &fee_payer,
            1000,
        );
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_sol_transfers() {
        let fee_payer = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();

        // Test with allow_sol_transfers = true
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.allow_transfer = true;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let instruction = transfer(&fee_payer, &recipient, 1000);

        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_sol_transfers = false
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.allow_transfer = false;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let instruction = transfer(&fee_payer, &recipient, 1000);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_assign() {
        let fee_payer = Pubkey::new_unique();
        let new_owner = Pubkey::new_unique();

        // Test with allow_assign = true

        let rpc_client = RpcMockBuilder::new().build();

        let mut policy = FeePayerPolicy::default();
        policy.system.allow_assign = true;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let instruction = assign(&fee_payer, &new_owner);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_assign = false

        let rpc_client = RpcMockBuilder::new().build();

        let mut policy = FeePayerPolicy::default();
        policy.system.allow_assign = false;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let instruction = assign(&fee_payer, &new_owner);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_spl_transfers() {
        let fee_payer = Pubkey::new_unique();

        let fee_payer_token_account = Pubkey::new_unique();
        let recipient_token_account = Pubkey::new_unique();

        // Test with allow_spl_transfers = true
        let rpc_client = RpcMockBuilder::new().build();

        let mut policy = FeePayerPolicy::default();
        policy.spl_token.allow_transfer = true;
        setup_spl_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let transfer_ix = spl_token_interface::instruction::transfer(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &recipient_token_account,
            &fee_payer, // fee payer is the signer
            &[],
            1000,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[transfer_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_spl_transfers = false
        let rpc_client = RpcMockBuilder::new().build();

        let mut policy = FeePayerPolicy::default();
        policy.spl_token.allow_transfer = false;
        setup_spl_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let transfer_ix = spl_token_interface::instruction::transfer(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &recipient_token_account,
            &fee_payer, // fee payer is the signer
            &[],
            1000,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[transfer_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());

        // Test with other account as source - should always pass
        let other_signer = Pubkey::new_unique();
        let transfer_ix = spl_token_interface::instruction::transfer(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &recipient_token_account,
            &other_signer, // other account is the signer
            &[],
            1000,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[transfer_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_token2022_transfers() {
        let fee_payer = Pubkey::new_unique();

        let fee_payer_token_account = Pubkey::new_unique();
        let recipient_token_account = Pubkey::new_unique();
        let mint = Pubkey::new_unique();

        // Test with allow_token2022_transfers = true
        let rpc_client = RpcMockBuilder::new()
            .with_mint_account(2) // Mock mint with 2 decimals for SPL outflow calculation
            .build();
        // Test with token_2022.allow_transfer = true
        let mut policy = FeePayerPolicy::default();
        policy.token_2022.allow_transfer = true;
        setup_token2022_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let transfer_ix = spl_token_2022_interface::instruction::transfer_checked(
            &spl_token_2022_interface::id(),
            &fee_payer_token_account,
            &mint,
            &recipient_token_account,
            &fee_payer, // fee payer is the signer
            &[],
            1,
            2,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[transfer_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_token2022_transfers = false
        let rpc_client = RpcMockBuilder::new()
            .with_mint_account(2) // Mock mint with 2 decimals for SPL outflow calculation
            .build();
        let mut policy = FeePayerPolicy::default();
        policy.token_2022.allow_transfer = false;
        setup_token2022_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let transfer_ix = spl_token_2022_interface::instruction::transfer_checked(
            &spl_token_2022_interface::id(),
            &fee_payer_token_account,
            &mint,
            &recipient_token_account,
            &fee_payer, // fee payer is the signer
            &[],
            1000,
            2,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[transfer_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        // Should fail because fee payer is not allowed to be source
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());

        // Test with other account as source - should always pass
        let other_signer = Pubkey::new_unique();
        let transfer_ix = spl_token_2022_interface::instruction::transfer_checked(
            &spl_token_2022_interface::id(),
            &fee_payer_token_account,
            &mint,
            &recipient_token_account,
            &other_signer, // other account is the signer
            &[],
            1000,
            2,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[transfer_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        // Should pass because fee payer is not the source
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn test_calculate_total_outflow() {
        let fee_payer = Pubkey::new_unique();
        let config = ConfigMockBuilder::new()
            .with_price_source(PriceSource::Mock)
            .with_allowed_programs(vec![SYSTEM_PROGRAM_ID.to_string()])
            .with_max_allowed_lamports(10_000_000)
            .with_fee_payer_policy(FeePayerPolicy::default())
            .build();
        update_config(config).unwrap();

        let rpc_client = RpcMockBuilder::new().build();
        let validator = TransactionValidator::new(fee_payer).unwrap();

        // Test 1: Fee payer as sender in Transfer - should add to outflow
        let recipient = Pubkey::new_unique();
        let transfer_instruction = transfer(&fee_payer, &recipient, 100_000);
        let message =
            VersionedMessage::Legacy(Message::new(&[transfer_instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        let outflow =
            validator.calculate_total_outflow(&mut transaction, &rpc_client).await.unwrap();
        assert_eq!(outflow, 100_000, "Transfer from fee payer should add to outflow");

        // Test 2: Fee payer as recipient in Transfer - should subtract from outflow (account closure)
        let sender = Pubkey::new_unique();
        let transfer_instruction = transfer(&sender, &fee_payer, 50_000);
        let message =
            VersionedMessage::Legacy(Message::new(&[transfer_instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        let outflow =
            validator.calculate_total_outflow(&mut transaction, &rpc_client).await.unwrap();
        assert_eq!(outflow, 0, "Transfer to fee payer should subtract from outflow"); // 0 - 50_000 = 0 (saturating_sub)

        // Test 3: Fee payer as funding account in CreateAccount - should add to outflow
        let new_account = Pubkey::new_unique();
        let create_instruction = create_account(
            &fee_payer,
            &new_account,
            200_000, // lamports
            100,     // space
            &SYSTEM_PROGRAM_ID,
        );
        let message =
            VersionedMessage::Legacy(Message::new(&[create_instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        let outflow =
            validator.calculate_total_outflow(&mut transaction, &rpc_client).await.unwrap();
        assert_eq!(outflow, 200_000, "CreateAccount funded by fee payer should add to outflow");

        // Test 4: Fee payer as funding account in CreateAccountWithSeed - should add to outflow
        let create_with_seed_instruction = create_account_with_seed(
            &fee_payer,
            &new_account,
            &fee_payer,
            "test_seed",
            300_000, // lamports
            100,     // space
            &SYSTEM_PROGRAM_ID,
        );
        let message = VersionedMessage::Legacy(Message::new(
            &[create_with_seed_instruction],
            Some(&fee_payer),
        ));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        let outflow =
            validator.calculate_total_outflow(&mut transaction, &rpc_client).await.unwrap();
        assert_eq!(
            outflow, 300_000,
            "CreateAccountWithSeed funded by fee payer should add to outflow"
        );

        // Test 5: TransferWithSeed from fee payer - should add to outflow
        let transfer_with_seed_instruction = transfer_with_seed(
            &fee_payer,
            &fee_payer,
            "test_seed".to_string(),
            &SYSTEM_PROGRAM_ID,
            &recipient,
            150_000,
        );
        let message = VersionedMessage::Legacy(Message::new(
            &[transfer_with_seed_instruction],
            Some(&fee_payer),
        ));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        let outflow =
            validator.calculate_total_outflow(&mut transaction, &rpc_client).await.unwrap();
        assert_eq!(outflow, 150_000, "TransferWithSeed from fee payer should add to outflow");

        // Test 6: Multiple instructions - should sum correctly
        let instructions = vec![
            transfer(&fee_payer, &recipient, 100_000), // +100_000
            transfer(&sender, &fee_payer, 30_000),     // -30_000
            create_account(&fee_payer, &new_account, 50_000, 100, &SYSTEM_PROGRAM_ID), // +50_000
        ];
        let message = VersionedMessage::Legacy(Message::new(&instructions, Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        let outflow =
            validator.calculate_total_outflow(&mut transaction, &rpc_client).await.unwrap();
        assert_eq!(
            outflow, 120_000,
            "Multiple instructions should sum correctly: 100000 - 30000 + 50000 = 120000"
        );

        // Test 7: Other account as sender - should not affect outflow
        let other_sender = Pubkey::new_unique();
        let transfer_instruction = transfer(&other_sender, &recipient, 500_000);
        let message =
            VersionedMessage::Legacy(Message::new(&[transfer_instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        let outflow =
            validator.calculate_total_outflow(&mut transaction, &rpc_client).await.unwrap();
        assert_eq!(outflow, 0, "Transfer from other account should not affect outflow");

        // Test 8: Other account funding CreateAccount - should not affect outflow
        let other_funder = Pubkey::new_unique();
        let create_instruction =
            create_account(&other_funder, &new_account, 1_000_000, 100, &SYSTEM_PROGRAM_ID);
        let message =
            VersionedMessage::Legacy(Message::new(&[create_instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        let outflow =
            validator.calculate_total_outflow(&mut transaction, &rpc_client).await.unwrap();
        assert_eq!(outflow, 0, "CreateAccount funded by other account should not affect outflow");
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_burn() {
        let fee_payer = Pubkey::new_unique();
        let fee_payer_token_account = Pubkey::new_unique();
        let mint = Pubkey::new_unique();

        // Test with allow_burn = true

        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.spl_token.allow_burn = true;
        setup_spl_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let burn_ix = spl_token_interface::instruction::burn(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &mint,
            &fee_payer,
            &[],
            1000,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[burn_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        // Should pass because allow_burn is true by default
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_burn = false

        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.spl_token.allow_burn = false;
        setup_spl_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let burn_ix = spl_token_interface::instruction::burn(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &mint,
            &fee_payer,
            &[],
            1000,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[burn_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        // Should fail because fee payer cannot burn tokens when allow_burn is false
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());

        // Test burn_checked instruction
        let burn_checked_ix = spl_token_interface::instruction::burn_checked(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &mint,
            &fee_payer,
            &[],
            1000,
            2,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[burn_checked_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        // Should also fail for burn_checked
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_close_account() {
        let fee_payer = Pubkey::new_unique();
        let fee_payer_token_account = Pubkey::new_unique();
        let destination = Pubkey::new_unique();

        // Test with allow_close_account = true

        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.spl_token.allow_close_account = true;
        setup_spl_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let close_ix = spl_token_interface::instruction::close_account(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &destination,
            &fee_payer,
            &[],
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[close_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        // Should pass because allow_close_account is true by default
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_close_account = false
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.spl_token.allow_close_account = false;
        setup_spl_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let close_ix = spl_token_interface::instruction::close_account(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &destination,
            &fee_payer,
            &[],
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[close_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        // Should fail because fee payer cannot close accounts when allow_close_account is false
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_approve() {
        let fee_payer = Pubkey::new_unique();
        let fee_payer_token_account = Pubkey::new_unique();
        let delegate = Pubkey::new_unique();

        // Test with allow_approve = true

        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.spl_token.allow_approve = true;
        setup_spl_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let approve_ix = spl_token_interface::instruction::approve(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &delegate,
            &fee_payer,
            &[],
            1000,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[approve_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        // Should pass because allow_approve is true by default
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_approve = false
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.spl_token.allow_approve = false;
        setup_spl_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let approve_ix = spl_token_interface::instruction::approve(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &delegate,
            &fee_payer,
            &[],
            1000,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[approve_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        // Should fail because fee payer cannot approve when allow_approve is false
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());

        // Test approve_checked instruction
        let mint = Pubkey::new_unique();
        let approve_checked_ix = spl_token_interface::instruction::approve_checked(
            &spl_token_interface::id(),
            &fee_payer_token_account,
            &mint,
            &delegate,
            &fee_payer,
            &[],
            1000,
            2,
        )
        .unwrap();

        let message =
            VersionedMessage::Legacy(Message::new(&[approve_checked_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        // Should also fail for approve_checked
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_token2022_burn() {
        let fee_payer = Pubkey::new_unique();
        let fee_payer_token_account = Pubkey::new_unique();
        let mint = Pubkey::new_unique();

        // Test with allow_burn = false for Token2022

        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.token_2022.allow_burn = false;
        setup_token2022_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let burn_ix = spl_token_2022_interface::instruction::burn(
            &spl_token_2022_interface::id(),
            &fee_payer_token_account,
            &mint,
            &fee_payer,
            &[],
            1000,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[burn_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        // Should fail for Token2022 burn
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_token2022_close_account() {
        let fee_payer = Pubkey::new_unique();
        let fee_payer_token_account = Pubkey::new_unique();
        let destination = Pubkey::new_unique();

        // Test with allow_close_account = false for Token2022

        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.token_2022.allow_close_account = false;
        setup_token2022_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let close_ix = spl_token_2022_interface::instruction::close_account(
            &spl_token_2022_interface::id(),
            &fee_payer_token_account,
            &destination,
            &fee_payer,
            &[],
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[close_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        // Should fail for Token2022 close account
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_token2022_approve() {
        let fee_payer = Pubkey::new_unique();
        let fee_payer_token_account = Pubkey::new_unique();
        let delegate = Pubkey::new_unique();

        // Test with allow_approve = true

        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.token_2022.allow_approve = true;
        setup_token2022_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let approve_ix = spl_token_2022_interface::instruction::approve(
            &spl_token_2022_interface::id(),
            &fee_payer_token_account,
            &delegate,
            &fee_payer,
            &[],
            1000,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[approve_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        // Should pass because allow_approve is true by default
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_approve = false

        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.token_2022.allow_approve = false;
        setup_token2022_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();

        let approve_ix = spl_token_2022_interface::instruction::approve(
            &spl_token_2022_interface::id(),
            &fee_payer_token_account,
            &delegate,
            &fee_payer,
            &[],
            1000,
        )
        .unwrap();

        let message = VersionedMessage::Legacy(Message::new(&[approve_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        // Should fail because fee payer cannot approve when allow_approve is false
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());

        // Test approve_checked instruction
        let mint = Pubkey::new_unique();
        let approve_checked_ix = spl_token_2022_interface::instruction::approve_checked(
            &spl_token_2022_interface::id(),
            &fee_payer_token_account,
            &mint,
            &delegate,
            &fee_payer,
            &[],
            1000,
            2,
        )
        .unwrap();

        let message =
            VersionedMessage::Legacy(Message::new(&[approve_checked_ix], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();

        // Should also fail for approve_checked
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_create_account() {
        use solana_system_interface::instruction::create_account;

        let fee_payer = Pubkey::new_unique();
        let new_account = Pubkey::new_unique();
        let owner = Pubkey::new_unique();

        // Test with allow_create_account = true
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.allow_create_account = true;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = create_account(&fee_payer, &new_account, 1000, 100, &owner);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_create_account = false
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.allow_create_account = false;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = create_account(&fee_payer, &new_account, 1000, 100, &owner);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_allocate() {
        use solana_system_interface::instruction::allocate;

        let fee_payer = Pubkey::new_unique();

        // Test with allow_allocate = true
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.allow_allocate = true;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = allocate(&fee_payer, 100);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_allocate = false
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.allow_allocate = false;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = allocate(&fee_payer, 100);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_nonce_initialize() {
        use solana_system_interface::instruction::create_nonce_account;

        let fee_payer = Pubkey::new_unique();
        let nonce_account = Pubkey::new_unique();

        // Test with allow_initialize = true
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.nonce.allow_initialize = true;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instructions = create_nonce_account(&fee_payer, &nonce_account, &fee_payer, 1_000_000);
        // Only test the InitializeNonceAccount instruction (second one)
        let message =
            VersionedMessage::Legacy(Message::new(&[instructions[1].clone()], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_initialize = false
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.nonce.allow_initialize = false;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instructions = create_nonce_account(&fee_payer, &nonce_account, &fee_payer, 1_000_000);
        let message =
            VersionedMessage::Legacy(Message::new(&[instructions[1].clone()], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_nonce_advance() {
        use solana_system_interface::instruction::advance_nonce_account;

        let fee_payer = Pubkey::new_unique();
        let nonce_account = Pubkey::new_unique();

        // Test with allow_advance = true
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.nonce.allow_advance = true;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = advance_nonce_account(&nonce_account, &fee_payer);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_advance = false
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.nonce.allow_advance = false;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = advance_nonce_account(&nonce_account, &fee_payer);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_nonce_withdraw() {
        use solana_system_interface::instruction::withdraw_nonce_account;

        let fee_payer = Pubkey::new_unique();
        let nonce_account = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();

        // Test with allow_withdraw = true
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.nonce.allow_withdraw = true;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = withdraw_nonce_account(&nonce_account, &fee_payer, &recipient, 1000);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_withdraw = false
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.nonce.allow_withdraw = false;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = withdraw_nonce_account(&nonce_account, &fee_payer, &recipient, 1000);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_fee_payer_policy_nonce_authorize() {
        use solana_system_interface::instruction::authorize_nonce_account;

        let fee_payer = Pubkey::new_unique();
        let nonce_account = Pubkey::new_unique();
        let new_authority = Pubkey::new_unique();

        // Test with allow_authorize = true
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.nonce.allow_authorize = true;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = authorize_nonce_account(&nonce_account, &fee_payer, &new_authority);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_ok());

        // Test with allow_authorize = false
        let rpc_client = RpcMockBuilder::new().build();
        let mut policy = FeePayerPolicy::default();
        policy.system.nonce.allow_authorize = false;
        setup_config_with_policy(policy);

        let validator = TransactionValidator::new(fee_payer).unwrap();
        let instruction = authorize_nonce_account(&nonce_account, &fee_payer, &new_authority);
        let message = VersionedMessage::Legacy(Message::new(&[instruction], Some(&fee_payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        assert!(validator.validate_transaction(&mut transaction, &rpc_client).await.is_err());
    }

    #[test]
    #[serial]
    fn test_strict_pricing_total_exceeds_fixed() {
        let mut config = ConfigMockBuilder::new().build();
        config.validation.price.model = PriceModel::Fixed {
            amount: 5000,
            token: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            strict: true,
        };
        let _ = update_config(config);

        // Fixed price = 5000, but total = 3000 + 2000 + 5000 = 10000 > 5000
        let fee_calc = TotalFeeCalculation::new(5000, 3000, 2000, 5000, 0, 0);

        let result = TransactionValidator::validate_strict_pricing_with_fee(&fee_calc);

        assert!(result.is_err());
        if let Err(KoraError::ValidationError(msg)) = result {
            assert!(msg.contains("Strict pricing violation"));
            assert!(msg.contains("exceeds fixed price"));
        } else {
            panic!("Expected ValidationError");
        }
    }

    #[test]
    #[serial]
    fn test_strict_pricing_total_within_fixed() {
        let mut config = ConfigMockBuilder::new().build();
        config.validation.price.model = PriceModel::Fixed {
            amount: 5000,
            token: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            strict: true,
        };
        let _ = update_config(config);

        // Fixed price = 5000, total = 1000 + 1000 + 1000 = 3000 < 5000
        let fee_calc = TotalFeeCalculation::new(5000, 1000, 1000, 1000, 0, 0);

        let result = TransactionValidator::validate_strict_pricing_with_fee(&fee_calc);

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_strict_pricing_disabled() {
        let mut config = ConfigMockBuilder::new().build();
        config.validation.price.model = PriceModel::Fixed {
            amount: 5000,
            token: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            strict: false, // Disabled
        };
        let _ = update_config(config);

        let fee_calc = TotalFeeCalculation::new(5000, 10000, 0, 0, 0, 0);

        let result = TransactionValidator::validate_strict_pricing_with_fee(&fee_calc);

        assert!(result.is_ok(), "Should pass when strict=false");
    }

    #[test]
    #[serial]
    fn test_strict_pricing_with_margin_pricing() {
        use crate::{
            fee::price::PriceModel, state::update_config, tests::config_mock::ConfigMockBuilder,
        };

        let mut config = ConfigMockBuilder::new().build();
        config.validation.price.model = PriceModel::Margin { margin: 0.1 };
        let _ = update_config(config);

        let fee_calc = TotalFeeCalculation::new(5000, 10000, 0, 0, 0, 0);

        let result = TransactionValidator::validate_strict_pricing_with_fee(&fee_calc);

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_strict_pricing_exact_match() {
        use crate::{
            fee::price::PriceModel, state::update_config, tests::config_mock::ConfigMockBuilder,
        };

        let mut config = ConfigMockBuilder::new().build();
        config.validation.price.model = PriceModel::Fixed {
            amount: 5000,
            token: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            strict: true,
        };
        let _ = update_config(config);

        // Total exactly equals fixed price (5000 = 5000)
        let fee_calc = TotalFeeCalculation::new(5000, 2000, 1000, 2000, 0, 0);

        let result = TransactionValidator::validate_strict_pricing_with_fee(&fee_calc);

        assert!(result.is_ok(), "Should pass when total equals fixed price");
    }

    fn rpc_account(data: Vec<u8>, owner: Pubkey) -> serde_json::Value {
        json!({
            "data": [base64::engine::general_purpose::STANDARD.encode(data), "base64"],
            "executable": false,
            "lamports": 2_039_280,
            "owner": owner.to_string(),
            "rentEpoch": 0
        })
    }

    fn legacy_mint_data(decimals: u8) -> Vec<u8> {
        let mut data = vec![0_u8; 82];
        data[44] = decimals;
        data[45] = 1;
        data
    }

    fn legacy_token_data(mint: Pubkey, authority: Pubkey) -> Vec<u8> {
        let mut data = vec![0_u8; 165];
        data[..32].copy_from_slice(mint.as_ref());
        data[32..64].copy_from_slice(authority.as_ref());
        data[64..72].copy_from_slice(&1_000_000_u64.to_le_bytes());
        data[108] = 1;
        data
    }

    fn meteora_semantic_fixture(
        corrupt_pair_owner: bool,
        missing_destination: bool,
        without_memo: bool,
    ) -> (TransactionValidator, Instruction, std::sync::Arc<RpcClient>) {
        let payer = Pubkey::new_unique();
        let wallet = Pubkey::new_unique();
        let mint_x = Pubkey::new_unique();
        let mint_y = Pubkey::new_unique();
        let token_program = spl_token_interface::id();
        let program = Pubkey::from_str(METEORA_DLMM_PROGRAM_ID).unwrap();
        let base_key = Pubkey::new_unique();
        let mut ordered = [mint_x, mint_y];
        ordered.sort();
        let pair = Pubkey::find_program_address(
            &[base_key.as_ref(), ordered[0].as_ref(), ordered[1].as_ref()],
            &program,
        )
        .0;
        let vault_x = Pubkey::find_program_address(&[pair.as_ref(), mint_x.as_ref()], &program).0;
        let vault_y = Pubkey::find_program_address(&[pair.as_ref(), mint_y.as_ref()], &program).0;
        let user_x = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&wallet, &mint_x, &token_program);
        let user_y = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&wallet, &mint_y, &token_program);
        let oracle = Pubkey::find_program_address(&[b"oracle", pair.as_ref()], &program).0;
        let bins = [0_i64, 1_i64].map(|index| {
            Pubkey::find_program_address(
                &[b"bin_array", pair.as_ref(), &index.to_le_bytes()],
                &program,
            )
            .0
        });
        let mut metas = vec![
            AccountMeta::new(pair, false),
            AccountMeta::new(program, false),
            AccountMeta::new(vault_x, false),
            AccountMeta::new(vault_y, false),
            AccountMeta::new(user_x, false),
            AccountMeta::new(user_y, false),
            AccountMeta::new_readonly(mint_x, false),
            AccountMeta::new_readonly(mint_y, false),
            AccountMeta::new(oracle, false),
            AccountMeta::new(program, false),
            AccountMeta::new(wallet, true),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(Pubkey::from_str(MEMO_PROGRAM_ID).unwrap(), false),
            AccountMeta::new_readonly(
                Pubkey::find_program_address(&[b"__event_authority"], &program).0,
                false,
            ),
            AccountMeta::new(program, false),
            AccountMeta::new(bins[0], false),
            AccountMeta::new(bins[1], false),
        ];
        if without_memo {
            metas.remove(13);
        }
        let mut data = METEORA_SWAP2_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&1_000_u64.to_le_bytes());
        data.extend_from_slice(&900_u64.to_le_bytes());
        let instruction = Instruction::new_with_bytes(program, &data, metas);
        let mut pair_data = vec![0_u8; 904];
        pair_data[..8].copy_from_slice(&METEORA_LB_PAIR_DISCRIMINATOR);
        pair_data[88..120].copy_from_slice(mint_x.as_ref());
        pair_data[120..152].copy_from_slice(mint_y.as_ref());
        pair_data[152..184].copy_from_slice(vault_x.as_ref());
        pair_data[184..216].copy_from_slice(vault_y.as_ref());
        pair_data[552..584].copy_from_slice(oracle.as_ref());
        pair_data[784..816].copy_from_slice(base_key.as_ref());
        let mut oracle_data = vec![0_u8; 3232];
        oracle_data[..8].copy_from_slice(&METEORA_ORACLE_DISCRIMINATOR);
        let bin_data = |index: i64| {
            let mut data = vec![0_u8; 10136];
            data[..8].copy_from_slice(&METEORA_BIN_ARRAY_DISCRIMINATOR);
            data[8..16].copy_from_slice(&index.to_le_bytes());
            data[24..56].copy_from_slice(pair.as_ref());
            data
        };
        let filler = rpc_account(vec![], SYSTEM_PROGRAM_ID);
        let mut values = vec![
            rpc_account(pair_data, if corrupt_pair_owner { SYSTEM_PROGRAM_ID } else { program }),
            filler.clone(),
            rpc_account(legacy_token_data(mint_x, pair), token_program),
            rpc_account(legacy_token_data(mint_y, pair), token_program),
            rpc_account(legacy_token_data(mint_x, wallet), token_program),
            if missing_destination {
                serde_json::Value::Null
            } else {
                rpc_account(legacy_token_data(mint_y, wallet), token_program)
            },
            rpc_account(legacy_mint_data(6), token_program),
            rpc_account(legacy_mint_data(9), token_program),
            rpc_account(oracle_data, program),
            filler.clone(),
            filler.clone(),
            filler.clone(),
            filler.clone(),
            filler.clone(),
            filler.clone(),
            filler,
            rpc_account(bin_data(0), program),
            rpc_account(bin_data(1), program),
        ];
        if without_memo {
            values.remove(13);
        }
        let mut policy = FeePayerPolicy::default();
        policy.system.canonical_ata_creation.allowed_output_mints =
            vec![mint_x.to_string(), mint_y.to_string()];
        policy.system.swap = crate::config::SwapPolicy {
            enabled: true,
            approved_dex_families: vec!["METEORA_DLMM".to_string()],
        };
        setup_config_with_policy(policy);
        let mut mocks = HashMap::new();
        mocks.insert(
            RpcRequest::GetMultipleAccounts,
            json!({ "context": { "slot": 1 }, "value": values }),
        );
        let rpc = RpcMockBuilder::new().with_custom_mocks(mocks).build();
        (TransactionValidator::new(payer).unwrap(), instruction, rpc)
    }

    #[tokio::test]
    #[serial]
    async fn swap_meteora_semantics_accept_exact_shape_and_reject_role_or_state_mutation() {
        let (validator, instruction, rpc) = meteora_semantic_fixture(false, false, false);
        let wallet = instruction.accounts[10].pubkey;
        assert!(validator.validate_meteora_dlmm(&instruction, &rpc, wallet).await.is_ok());
        let (validator, mut legacy_swap, rpc) = meteora_semantic_fixture(false, false, true);
        let wallet = legacy_swap.accounts[10].pubkey;
        legacy_swap.data[..8].copy_from_slice(&METEORA_SWAP_DISCRIMINATOR);
        legacy_swap.data[16..24].fill(0);
        assert!(validator.validate_meteora_dlmm(&legacy_swap, &rpc, wallet).await.is_ok());
        let mut zero_input = legacy_swap.clone();
        zero_input.data[8..16].fill(0);
        assert!(validator.validate_meteora_dlmm(&zero_input, &rpc, wallet).await.is_err());
        let mut zero_bound_swap2 = instruction.clone();
        zero_bound_swap2.data[16..24].fill(0);
        assert!(validator
            .validate_meteora_dlmm(&zero_bound_swap2, &rpc, instruction.accounts[10].pubkey)
            .await
            .is_err());
        let mut wrong_role = instruction.clone();
        wrong_role.accounts[8].is_writable = false;
        assert!(validator.validate_meteora_dlmm(&wrong_role, &rpc, wallet).await.is_err());
        let (validator, instruction, rpc) = meteora_semantic_fixture(false, false, true);
        assert!(validator
            .validate_meteora_dlmm(&instruction, &rpc, instruction.accounts[10].pubkey)
            .await
            .is_ok());
        let (validator, instruction, rpc) = meteora_semantic_fixture(false, true, false);
        assert!(validator
            .validate_meteora_dlmm(&instruction, &rpc, instruction.accounts[10].pubkey)
            .await
            .is_ok());
        let (validator, instruction, rpc) = meteora_semantic_fixture(true, false, false);
        assert!(validator
            .validate_meteora_dlmm(&instruction, &rpc, instruction.accounts[10].pubkey)
            .await
            .is_err());
    }

    fn pumpswap_sell_semantic_fixture(
        corrupt_pool_owner: bool,
        missing_destination: bool,
        without_pool_v2: bool,
    ) -> (TransactionValidator, Instruction, std::sync::Arc<RpcClient>) {
        let payer = Pubkey::new_unique();
        let wallet = Pubkey::new_unique();
        let token_program = spl_token_interface::id();
        let program = Pubkey::from_str(PUMPSWAP_PROGRAM_ID).unwrap();
        let fee_program = Pubkey::from_str(PUMP_FEE_PROGRAM_ID).unwrap();
        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let creator = Pubkey::new_unique();
        let coin_creator = Pubkey::new_unique();
        let index = 1_u16;
        let pool = Pubkey::find_program_address(
            &[
                b"pool",
                &index.to_le_bytes(),
                creator.as_ref(),
                base_mint.as_ref(),
                quote_mint.as_ref(),
            ],
            &program,
        )
        .0;
        let ata = |owner: Pubkey, mint: Pubkey| {
            spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&owner, &mint, &token_program)
        };
        let user_base = ata(wallet, base_mint);
        let user_quote = ata(wallet, quote_mint);
        let pool_base = ata(pool, base_mint);
        let pool_quote = ata(pool, quote_mint);
        let protocol = Pubkey::new_unique();
        let protocol_ata = ata(protocol, quote_mint);
        let creator_authority =
            Pubkey::find_program_address(&[b"creator_vault", coin_creator.as_ref()], &program).0;
        let creator_ata = ata(creator_authority, quote_mint);
        let fee_config =
            Pubkey::find_program_address(&[b"fee_config", program.as_ref()], &fee_program).0;
        let pool_v2 = Pubkey::find_program_address(&[b"pool-v2", base_mint.as_ref()], &program).0;
        let current_fee_recipient =
            Pubkey::from_str("GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL").unwrap();
        let current_fee_ata = ata(current_fee_recipient, quote_mint);
        let mut metas = vec![
            AccountMeta::new(pool, false),
            AccountMeta::new(wallet, true),
            AccountMeta::new_readonly(Pubkey::from_str(PUMP_GLOBAL_CONFIG).unwrap(), false),
            AccountMeta::new_readonly(base_mint, false),
            AccountMeta::new_readonly(quote_mint, false),
            AccountMeta::new(user_base, false),
            AccountMeta::new(user_quote, false),
            AccountMeta::new(pool_base, false),
            AccountMeta::new(pool_quote, false),
            AccountMeta::new_readonly(protocol, false),
            AccountMeta::new(protocol_ata, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(spl_associated_token_account_interface::program::id(), false),
            AccountMeta::new_readonly(
                Pubkey::find_program_address(&[b"__event_authority"], &program).0,
                false,
            ),
            AccountMeta::new_readonly(program, false),
            AccountMeta::new(creator_ata, false),
            AccountMeta::new_readonly(creator_authority, false),
            AccountMeta::new_readonly(fee_config, false),
            AccountMeta::new_readonly(fee_program, false),
            AccountMeta::new_readonly(pool_v2, false),
            AccountMeta::new_readonly(current_fee_recipient, false),
            AccountMeta::new(current_fee_ata, false),
        ];
        if without_pool_v2 {
            metas.remove(21);
        }
        let mut data = PUMPSWAP_SELL_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&1_000_u64.to_le_bytes());
        data.extend_from_slice(&900_u64.to_le_bytes());
        let instruction = Instruction::new_with_bytes(program, &data, metas);
        let mut pool_data = vec![0_u8; 300];
        pool_data[..8].copy_from_slice(&PUMPSWAP_POOL_DISCRIMINATOR);
        pool_data[9..11].copy_from_slice(&index.to_le_bytes());
        pool_data[11..43].copy_from_slice(creator.as_ref());
        pool_data[43..75].copy_from_slice(base_mint.as_ref());
        pool_data[75..107].copy_from_slice(quote_mint.as_ref());
        pool_data[139..171].copy_from_slice(pool_base.as_ref());
        pool_data[171..203].copy_from_slice(pool_quote.as_ref());
        pool_data[211..243].copy_from_slice(coin_creator.as_ref());
        let mut global_data = vec![0_u8; 940];
        global_data[..8].copy_from_slice(&PUMPSWAP_GLOBAL_DISCRIMINATOR);
        global_data[57..89].copy_from_slice(protocol.as_ref());
        let filler = rpc_account(vec![], SYSTEM_PROGRAM_ID);
        let mut values = vec![
            rpc_account(pool_data, if corrupt_pool_owner { SYSTEM_PROGRAM_ID } else { program }),
            filler.clone(),
            rpc_account(global_data, program),
            rpc_account(legacy_mint_data(6), token_program),
            rpc_account(legacy_mint_data(9), token_program),
            rpc_account(legacy_token_data(base_mint, wallet), token_program),
            if missing_destination {
                serde_json::Value::Null
            } else {
                rpc_account(legacy_token_data(quote_mint, wallet), token_program)
            },
            rpc_account(legacy_token_data(base_mint, pool), token_program),
            rpc_account(legacy_token_data(quote_mint, pool), token_program),
            filler.clone(),
            rpc_account(legacy_token_data(quote_mint, protocol), token_program),
            filler.clone(),
            filler.clone(),
            filler.clone(),
            filler.clone(),
            filler.clone(),
            filler.clone(),
            rpc_account(legacy_token_data(quote_mint, creator_authority), token_program),
            filler.clone(),
            rpc_account(vec![0_u8; 4073], fee_program),
            filler.clone(),
            filler.clone(),
            filler,
            rpc_account(legacy_token_data(quote_mint, current_fee_recipient), token_program),
        ];
        if without_pool_v2 {
            values.remove(21);
        }
        let mut policy = FeePayerPolicy::default();
        policy.system.canonical_ata_creation.allowed_output_mints =
            vec![base_mint.to_string(), quote_mint.to_string()];
        policy.system.swap = crate::config::SwapPolicy {
            enabled: true,
            approved_dex_families: vec!["PUMPSWAP".to_string()],
        };
        setup_config_with_policy(policy);
        let mut mocks = HashMap::new();
        mocks.insert(
            RpcRequest::GetMultipleAccounts,
            json!({ "context": { "slot": 1 }, "value": values }),
        );
        let rpc = RpcMockBuilder::new().with_custom_mocks(mocks).build();
        (TransactionValidator::new(payer).unwrap(), instruction, rpc)
    }

    #[tokio::test]
    #[serial]
    async fn swap_pumpswap_semantics_accept_exact_sell_and_reject_fee_or_state_mutation() {
        let (validator, instruction, rpc) = pumpswap_sell_semantic_fixture(false, false, false);
        let wallet = instruction.accounts[1].pubkey;
        assert!(validator.validate_pumpswap(&instruction, &rpc, wallet).await.is_ok());
        let mut wrong_fee = instruction.clone();
        wrong_fee.accounts[22].pubkey = Pubkey::new_unique();
        assert!(validator.validate_pumpswap(&wrong_fee, &rpc, wallet).await.is_err());
        let (validator, instruction, rpc) = pumpswap_sell_semantic_fixture(false, false, true);
        assert!(validator
            .validate_pumpswap(&instruction, &rpc, instruction.accounts[1].pubkey)
            .await
            .is_ok());
        let (validator, instruction, rpc) = pumpswap_sell_semantic_fixture(false, true, false);
        assert!(validator
            .validate_pumpswap(&instruction, &rpc, instruction.accounts[1].pubkey)
            .await
            .is_ok());
        let (validator, instruction, rpc) = pumpswap_sell_semantic_fixture(true, false, false);
        assert!(validator
            .validate_pumpswap(&instruction, &rpc, instruction.accounts[1].pubkey)
            .await
            .is_err());
    }

    fn swap_route_transaction(
        payer: Pubkey,
        wallet: Pubkey,
        dex: Instruction,
    ) -> VersionedTransactionResolved {
        let mut route_accounts = dex.accounts.clone();
        if !route_accounts.iter().any(|meta| meta.pubkey == dex.program_id) {
            route_accounts.push(AccountMeta::new_readonly(dex.program_id, false));
        }
        let route = Instruction::new_with_bytes(
            Pubkey::from_str(JUPITER_V6_PROGRAM_ID).unwrap(),
            &[1],
            route_accounts,
        );
        let message = VersionedMessage::Legacy(Message::new(&[route], Some(&payer)));
        let mut transaction =
            TransactionUtil::new_unsigned_versioned_transaction_resolved(message).unwrap();
        transaction.all_instructions.push(dex.clone());
        transaction.inner_instruction_contexts.push(InnerInstructionContext {
            instruction: dex,
            outer_instruction_index: 0,
            stack_height: Some(2),
        });
        assert_eq!(transaction.transaction.message.static_account_keys()[1], wallet);
        transaction
    }

    #[tokio::test]
    #[serial]
    async fn swap_route_dispatch_accepts_one_approved_family_and_rejects_mixed_direct_legs() {
        let (validator, meteora, rpc) = meteora_semantic_fixture(false, false, false);
        let wallet = meteora.accounts[10].pubkey;
        let transaction = swap_route_transaction(validator.fee_payer_pubkey, wallet, meteora);
        assert!(validator.validate_swap_route(&transaction, &rpc).await.is_ok());

        let (validator, pumpswap, rpc) = pumpswap_sell_semantic_fixture(false, false, false);
        let wallet = pumpswap.accounts[1].pubkey;
        let mut transaction =
            swap_route_transaction(validator.fee_payer_pubkey, wallet, pumpswap.clone());
        let mut foreign = pumpswap;
        foreign.program_id = Pubkey::from_str(METEORA_DLMM_PROGRAM_ID).unwrap();
        transaction.all_instructions.push(foreign.clone());
        transaction.inner_instruction_contexts.push(InnerInstructionContext {
            instruction: foreign,
            outer_instruction_index: 0,
            stack_height: Some(2),
        });
        assert!(validator.validate_swap_route(&transaction, &rpc).await.is_err());
    }
}
