use crate::{
    config::RecoverPolicy,
    error::KoraError,
    transaction::{RecoverAuthorizationClaims, VersionedTransactionResolved},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_PROGRAM_ID};
use std::{
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};
use utoipa::ToSchema;

const JUPITER_V6_PROGRAM_ID: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
const WRAPPED_SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const CLOCK_SKEW_SECONDS: u64 = 30;

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RecoverAuthorization {
    pub payload: String,
    pub signature: String,
}

pub fn is_recover_transaction(transaction: &VersionedTransactionResolved) -> bool {
    let Ok(jupiter) = Pubkey::from_str(JUPITER_V6_PROGRAM_ID) else {
        return false;
    };
    let outer_count = transaction.transaction.message.instructions().len();
    let Some(outer) = transaction.all_instructions.get(..outer_count) else {
        return false;
    };
    outer.iter().any(|instruction| instruction.program_id == jupiter)
        && outer
            .iter()
            .filter(|instruction| {
                instruction.program_id == spl_token_interface::id()
                    && matches!(instruction.data.first(), Some(9))
            })
            .count()
            == 2
}

pub fn validate_recover_authorization(
    transaction: &VersionedTransactionResolved,
    policy: &RecoverPolicy,
    authorization: Option<&RecoverAuthorization>,
) -> Result<Option<RecoverAuthorizationClaims>, KoraError> {
    if !is_recover_transaction(transaction) {
        if authorization.is_some() {
            return Err(KoraError::InvalidTransaction(
                "Recover authorization cannot authorize a non-Recover transaction".to_string(),
            ));
        }
        return Ok(None);
    }
    if !policy.enabled {
        return Err(KoraError::InvalidTransaction("Recover Value is disabled".to_string()));
    }
    let authorization = authorization.ok_or_else(|| {
        KoraError::InvalidTransaction("Recover server authorization is required".to_string())
    })?;
    let payload = URL_SAFE_NO_PAD.decode(&authorization.payload).map_err(|_| {
        KoraError::InvalidTransaction("Recover authorization payload is invalid".to_string())
    })?;
    let verification_key =
        Pubkey::from_str(&policy.authorization_public_key).map_err(|_| KoraError::ConfigError)?;
    let signature = Signature::from_str(&authorization.signature).map_err(|_| {
        KoraError::InvalidTransaction("Recover authorization signature is invalid".to_string())
    })?;
    if !signature.verify(verification_key.as_ref(), &payload) {
        return Err(KoraError::InvalidTransaction(
            "Recover authorization signature verification failed".to_string(),
        ));
    }
    let claims: RecoverAuthorizationClaims = serde_json::from_slice(&payload).map_err(|_| {
        KoraError::InvalidTransaction("Recover authorization claims are invalid".to_string())
    })?;
    validate_claims(transaction, policy, &claims)?;
    Ok(Some(claims))
}

fn validate_claims(
    transaction: &VersionedTransactionResolved,
    policy: &RecoverPolicy,
    claims: &RecoverAuthorizationClaims,
) -> Result<(), KoraError> {
    let parse_amount = |value: &str| {
        value.parse::<u64>().map_err(|_| {
            KoraError::InvalidTransaction("Recover authorization amount is invalid".to_string())
        })
    };
    let input = parse_amount(&claims.input_amount_raw)?;
    let expected = parse_amount(&claims.expected_output_lamports)?;
    let minimum = parse_amount(&claims.minimum_output_lamports)?;
    let minimum_user_payout = parse_amount(&claims.minimum_user_payout_lamports)?;
    let swap_fee = parse_amount(&claims.swap_fee_lamports)?;
    let rent_fee = parse_amount(&claims.rent_fee_lamports)?;
    let network = parse_amount(&claims.network_reimbursement_lamports)?;
    let setup = parse_amount(&claims.setup_rent_reimbursement_lamports)?;
    let sponsored = parse_amount(&claims.sponsored_cost_lamports)?;
    if claims.schema_version != "recover-authorization-v1"
        || claims.action != "CLEAN_RECOVER"
        || claims.network != policy.authorization_network
        || claims.pilot_wallet != policy.user_wallet
        || claims.source_token_account != policy.source_account
        || claims.input_mint != policy.input_mint
        || claims.output_mint != WRAPPED_SOL_MINT
        || claims.treasury != policy.settlement_wallet
        || claims.quote_id.is_empty()
        || claims.intent_id.is_empty()
        || claims.nonce.is_empty()
        || minimum_user_payout < policy.minimum_user_payout_lamports
        || sponsored != network.checked_add(setup).ok_or(KoraError::ConfigError)?
    {
        return Err(KoraError::InvalidTransaction(
            "Recover authorization scope or identities are invalid".to_string(),
        ));
    }
    let message_hash = hex::encode(Sha256::digest(transaction.transaction.message.serialize()));
    if claims.message_hash != message_hash {
        return Err(KoraError::InvalidTransaction(
            "Recover authorization does not match the exact transaction message".to_string(),
        ));
    }
    let outer_count = transaction.transaction.message.instructions().len();
    let outer = transaction.all_instructions.get(..outer_count).ok_or_else(|| {
        KoraError::InvalidTransaction(
            "Recover authorization instructions are unresolved".to_string(),
        )
    })?;
    let jupiter = Pubkey::from_str(JUPITER_V6_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
    let route =
        outer.iter().find(|instruction| instruction.program_id == jupiter).ok_or_else(|| {
            KoraError::InvalidTransaction("Recover authorization route is missing".to_string())
        })?;
    if route.data.len() != 39 {
        return Err(KoraError::InvalidTransaction(
            "Recover authorization route is invalid".to_string(),
        ));
    }
    let actual_input =
        u64::from_le_bytes(route.data[8..16].try_into().map_err(|_| KoraError::ConfigError)?);
    let actual_expected =
        u64::from_le_bytes(route.data[16..24].try_into().map_err(|_| KoraError::ConfigError)?);
    let slippage =
        u16::from_le_bytes(route.data[24..26].try_into().map_err(|_| KoraError::ConfigError)?);
    let actual_minimum = actual_expected
        .checked_mul(u64::from(10_000u16.checked_sub(slippage).ok_or(KoraError::ConfigError)?))
        .and_then(|value| value.checked_add(9_999))
        .ok_or(KoraError::ConfigError)?
        / 10_000;
    let settlement = outer
        .iter()
        .find_map(|instruction| {
            if instruction.program_id != SYSTEM_PROGRAM_ID {
                return None;
            }
            match bincode::deserialize::<SystemInstruction>(&instruction.data) {
                Ok(SystemInstruction::Transfer { lamports }) => Some(lamports),
                _ => None,
            }
        })
        .ok_or_else(|| {
            KoraError::InvalidTransaction("Recover authorization settlement is missing".to_string())
        })?;
    let claimed_settlement = swap_fee
        .checked_add(rent_fee)
        .and_then(|value| value.checked_add(network))
        .and_then(|value| value.checked_add(setup))
        .ok_or(KoraError::ConfigError)?;
    if input != actual_input
        || expected != actual_expected
        || minimum != actual_minimum
        || settlement != claimed_settlement
    {
        return Err(KoraError::InvalidTransaction(
            "Recover authorization economics do not match the transaction".to_string(),
        ));
    }
    let now =
        SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| KoraError::ConfigError)?.as_secs();
    let lifetime = claims
        .expires_at_unix_seconds
        .checked_sub(claims.issued_at_unix_seconds)
        .ok_or_else(|| {
            KoraError::InvalidTransaction("Recover authorization lifetime is invalid".to_string())
        })?;
    if lifetime == 0
        || lifetime > policy.authorization_max_lifetime_seconds
        || claims.issued_at_unix_seconds > now.saturating_add(CLOCK_SKEW_SECONDS)
        || claims.expires_at_unix_seconds <= now
    {
        return Err(KoraError::InvalidTransaction(
            "Recover authorization is expired or outside its allowed lifetime".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::VersionedTransactionResolved;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use solana_message::{v0, VersionedMessage};
    use solana_sdk::{
        hash::Hash,
        instruction::{AccountMeta, Instruction},
        signature::{Keypair, Signer},
        transaction::VersionedTransaction,
    };
    use solana_system_interface::instruction as system_instruction;

    struct Fixture {
        authority: Keypair,
        payer: Keypair,
        policy: RecoverPolicy,
        transaction: VersionedTransactionResolved,
    }

    fn fixture(settlement_lamports: u64, blockhash: Hash) -> Fixture {
        let authority = Keypair::new();
        let payer = Keypair::new();
        let wallet = Pubkey::new_unique();
        let treasury = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let source = Pubkey::new_unique();
        let wrapped = Pubkey::new_unique();
        let jupiter = Pubkey::from_str(JUPITER_V6_PROGRAM_ID).unwrap();
        let mut route_data = vec![0u8; 39];
        route_data[..8].copy_from_slice(&[187, 100, 250, 204, 49, 196, 175, 20]);
        route_data[8..16].copy_from_slice(&500u64.to_le_bytes());
        route_data[16..24].copy_from_slice(&1_000u64.to_le_bytes());
        route_data[24..26].copy_from_slice(&50u16.to_le_bytes());
        let close = |account| {
            spl_token_interface::instruction::close_account(
                &spl_token_interface::id(),
                &account,
                &wallet,
                &wallet,
                &[],
            )
            .unwrap()
        };
        let instructions = vec![
            Instruction {
                program_id: jupiter,
                accounts: vec![AccountMeta::new(source, false)],
                data: route_data,
            },
            close(source),
            close(wrapped),
            system_instruction::transfer(&wallet, &treasury, settlement_lamports),
        ];
        let message =
            v0::Message::try_compile(&payer.pubkey(), &instructions, &[], blockhash).unwrap();
        let transaction =
            VersionedTransactionResolved::from_kora_built_transaction(&VersionedTransaction {
                signatures: vec![Signature::default(), Signature::default()],
                message: VersionedMessage::V0(message),
            })
            .unwrap();
        let policy = RecoverPolicy {
            enabled: true,
            user_wallet: wallet.to_string(),
            settlement_wallet: treasury.to_string(),
            input_mint: mint.to_string(),
            source_account: source.to_string(),
            wrapped_sol_account: wrapped.to_string(),
            decimals: 6,
            authorization_public_key: authority.pubkey().to_string(),
            authorization_network: "mainnet-beta".to_string(),
            authorization_max_lifetime_seconds: 90,
            ..RecoverPolicy::default()
        };
        Fixture { authority, payer, policy, transaction }
    }

    fn authorize(fixture: &Fixture, issued_at: u64) -> RecoverAuthorization {
        let message_hash =
            hex::encode(Sha256::digest(fixture.transaction.transaction.message.serialize()));
        let claims = RecoverAuthorizationClaims {
            schema_version: "recover-authorization-v1".to_string(),
            action: "CLEAN_RECOVER".to_string(),
            network: "mainnet-beta".to_string(),
            pilot_wallet: fixture.policy.user_wallet.clone(),
            source_token_account: fixture.policy.source_account.clone(),
            input_mint: fixture.policy.input_mint.clone(),
            input_amount_raw: "500".to_string(),
            output_mint: WRAPPED_SOL_MINT.to_string(),
            expected_output_lamports: "1000".to_string(),
            minimum_output_lamports: "995".to_string(),
            minimum_user_payout_lamports: "1".to_string(),
            swap_fee_lamports: "10".to_string(),
            rent_fee_lamports: "20".to_string(),
            network_reimbursement_lamports: "30".to_string(),
            setup_rent_reimbursement_lamports: "40".to_string(),
            sponsored_cost_lamports: "70".to_string(),
            treasury: fixture.policy.settlement_wallet.clone(),
            message_hash,
            quote_id: Pubkey::new_unique().to_string(),
            intent_id: Pubkey::new_unique().to_string(),
            nonce: Pubkey::new_unique().to_string(),
            issued_at_unix_seconds: issued_at,
            expires_at_unix_seconds: issued_at + 60,
        };
        let payload = serde_json::to_vec(&claims).unwrap();
        RecoverAuthorization {
            payload: URL_SAFE_NO_PAD.encode(&payload),
            signature: fixture.authority.sign_message(&payload).to_string(),
        }
    }

    fn now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    #[test]
    fn exact_fresh_recover_message_is_authorized() {
        let fixture = fixture(100, Hash::new_unique());
        let authorization = authorize(&fixture, now());
        assert!(validate_recover_authorization(
            &fixture.transaction,
            &fixture.policy,
            Some(&authorization)
        )
        .is_ok());
        let message = fixture.transaction.transaction.message.serialize();
        let payer_signature = fixture.payer.sign_message(&message);
        assert!(payer_signature.verify(fixture.payer.pubkey().as_ref(), &message));
        assert!(
            validate_recover_authorization(&fixture.transaction, &fixture.policy, None).is_err()
        );
    }

    #[test]
    fn one_lamport_message_mutation_requires_new_authorization() {
        let original = fixture(100, Hash::new_unique());
        let authorization = authorize(&original, now());
        let mut mutated = original.transaction.clone();
        if let VersionedMessage::V0(message) = &mut mutated.transaction.message {
            message.instructions[0].data[16..24].copy_from_slice(&999u64.to_le_bytes());
        }
        mutated.all_instructions[0].data[16..24].copy_from_slice(&999u64.to_le_bytes());
        assert!(validate_recover_authorization(&mutated, &original.policy, Some(&authorization))
            .is_err());
    }

    #[test]
    fn expired_authorization_is_rejected() {
        let fixture = fixture(100, Hash::new_unique());
        let authorization = authorize(&fixture, now() - 120);
        assert!(validate_recover_authorization(
            &fixture.transaction,
            &fixture.policy,
            Some(&authorization)
        )
        .is_err());
    }

    #[test]
    fn authorization_scope_signature_and_payload_mutation_matrix_is_rejected() {
        let fixture = fixture(100, Hash::new_unique());
        let valid = authorize(&fixture, now());
        let original: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&valid.payload).unwrap()).unwrap();
        let mutations = [
            ("message_hash", serde_json::json!("00")),
            ("expected_output_lamports", serde_json::json!("999")),
            ("minimum_output_lamports", serde_json::json!("994")),
            ("input_amount_raw", serde_json::json!("499")),
            ("source_token_account", serde_json::json!(Pubkey::new_unique().to_string())),
            ("pilot_wallet", serde_json::json!(Pubkey::new_unique().to_string())),
            ("input_mint", serde_json::json!(Pubkey::new_unique().to_string())),
            ("treasury", serde_json::json!(Pubkey::new_unique().to_string())),
            ("swap_fee_lamports", serde_json::json!("11")),
            ("action", serde_json::json!("SWAP")),
            ("network", serde_json::json!("devnet")),
        ];
        for (field, value) in mutations {
            let mut claims = original.clone();
            claims[field] = value;
            let payload = serde_json::to_vec(&claims).unwrap();
            let authorization = RecoverAuthorization {
                payload: URL_SAFE_NO_PAD.encode(&payload),
                signature: fixture.authority.sign_message(&payload).to_string(),
            };
            assert!(
                validate_recover_authorization(
                    &fixture.transaction,
                    &fixture.policy,
                    Some(&authorization)
                )
                .is_err(),
                "signed authorization mutation {field} must fail"
            );
        }
        let mut wrong_signature = valid.clone();
        wrong_signature.signature = Keypair::new().sign_message(b"wrong").to_string();
        assert!(validate_recover_authorization(
            &fixture.transaction,
            &fixture.policy,
            Some(&wrong_signature)
        )
        .is_err());
        let malformed_payload = b"not-json";
        let malformed = RecoverAuthorization {
            payload: URL_SAFE_NO_PAD.encode(malformed_payload),
            signature: fixture.authority.sign_message(malformed_payload).to_string(),
        };
        assert!(validate_recover_authorization(
            &fixture.transaction,
            &fixture.policy,
            Some(&malformed)
        )
        .is_err());
        let mut wrong_key_policy = fixture.policy.clone();
        wrong_key_policy.authorization_public_key = Pubkey::new_unique().to_string();
        assert!(validate_recover_authorization(
            &fixture.transaction,
            &wrong_key_policy,
            Some(&valid)
        )
        .is_err());
    }

    #[test]
    fn two_fresh_messages_accept_independent_authorizations_without_policy_change() {
        let first = fixture(100, Hash::new_unique());
        let second_transaction = fixture(100, Hash::new_unique());
        let mut second = Fixture {
            authority: first.authority.insecure_clone(),
            payer: first.payer.insecure_clone(),
            policy: first.policy.clone(),
            transaction: second_transaction.transaction,
        };
        second.policy.user_wallet = first.policy.user_wallet.clone();
        second.policy.source_account = first.policy.source_account.clone();
        second.policy.input_mint = first.policy.input_mint.clone();
        second.policy.settlement_wallet = first.policy.settlement_wallet.clone();
        // Recompile using the same identities while changing only the fresh blockhash.
        let outer = first.transaction.all_instructions.clone();
        let payer = first.transaction.transaction.message.static_account_keys()[0];
        let message = v0::Message::try_compile(&payer, &outer, &[], Hash::new_unique()).unwrap();
        second.transaction =
            VersionedTransactionResolved::from_kora_built_transaction(&VersionedTransaction {
                signatures: vec![Signature::default(), Signature::default()],
                message: VersionedMessage::V0(message),
            })
            .unwrap();
        let first_authorization = authorize(&first, now());
        let second_authorization = authorize(&second, now());
        assert!(validate_recover_authorization(
            &first.transaction,
            &first.policy,
            Some(&first_authorization)
        )
        .is_ok());
        assert!(validate_recover_authorization(
            &second.transaction,
            &second.policy,
            Some(&second_authorization)
        )
        .is_ok());
        assert_ne!(first_authorization.payload, second_authorization.payload);
    }
}
