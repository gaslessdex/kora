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
use solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction};
use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_PROGRAM_ID};
use std::str::FromStr;

use crate::fee::price::PriceModel;

const JUPITER_V6_PROGRAM_ID: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
const RAYDIUM_CLMM_PROGRAM_ID: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
const SEND_COMPUTE_UNIT_LIMIT: u32 = 200_000;
const SEND_COMPUTE_UNIT_PRICE_MICROLAMPORTS: u64 = 375_000;

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
        if payer_creations > 0 && !self.fee_payer_policy.system.allow_create_account {
            if has_outer_jupiter {
                self.validate_canonical_ata_creation(
                    transaction_resolved,
                    rpc_client,
                    payer_creations,
                )
                .await?;
            } else {
                self.validate_send(transaction_resolved, rpc_client, payer_creations).await?;
            }
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
        let raydium_program =
            Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
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
        let has_outer_raydium = transaction
            .all_instructions
            .iter()
            .take(outer_instruction_count)
            .any(|instruction| instruction.program_id == raydium_program);
        if outer_jupiter_indices.len() != 1 || has_outer_raydium {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA exception requires one outer Jupiter v6 swap".to_string(),
            ));
        }
        let jupiter_index = outer_jupiter_indices[0];
        let raydium_contexts = transaction
            .inner_instruction_contexts
            .iter()
            .filter(|context| context.instruction.program_id == raydium_program)
            .collect::<Vec<_>>();
        if raydium_contexts.is_empty()
            || raydium_contexts.iter().any(|context| {
                context.outer_instruction_index as usize != jupiter_index
                    || context.stack_height != Some(2)
            })
        {
            return Err(KoraError::InvalidTransaction(
                "Canonical ATA exception requires direct Jupiter to Raydium CLMM CPI".to_string(),
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
        config::{CanonicalAtaCreationPolicy, FeePayerPolicy, SendMintPolicy, SendPolicy},
        state::update_config,
        tests::{config_mock::ConfigMockBuilder, rpc_mock::RpcMockBuilder},
        transaction::{InnerInstructionContext, TransactionUtil},
    };
    use base64::Engine;
    use serial_test::serial;

    use super::*;
    use serde_json::json;
    use solana_client::rpc_request::RpcRequest;
    use solana_message::{Message, VersionedMessage};
    use solana_sdk::instruction::{AccountMeta, Instruction};
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
}
