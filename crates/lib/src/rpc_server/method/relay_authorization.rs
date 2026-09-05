use crate::{
    config::RelayPolicy,
    error::KoraError,
    transaction::{RelayAuthorizationClaims, VersionedTransactionResolved},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_message::VersionedMessage;
use solana_sdk::{instruction::Instruction, pubkey::Pubkey, signature::Signature};
use std::{
    collections::HashMap,
    str::FromStr,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use utoipa::ToSchema;

const RELAY_PROGRAM_ID: &str = "99vQwtBwYtrqqD9YSXbdum3KBdxPAVxYTaQ3cfnJSrN2";
const RELAY_LOOKUP_TABLE: &str = "Hm9fUgcn7qwDaiNTFiGh6pNtVATgnaRcmK6Bbx6EMZfP";
const NATIVE_DISCRIMINATOR: [u8; 8] = [13, 158, 13, 223, 95, 213, 28, 6];
const TOKEN_DISCRIMINATOR: [u8; 8] = [11, 156, 96, 218, 39, 163, 180, 19];
const CLOCK_SKEW_SECONDS: u64 = 30;
const SYSTEM_PROGRAM_ID: &str = "11111111111111111111111111111111";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDT_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
static USED_NONCES: Lazy<Mutex<HashMap<String, u64>>> = Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RelayAuthorization {
    pub payload: String,
    pub signature: String,
}

pub fn validate_relay_authorization(
    transaction: &VersionedTransactionResolved,
    payer: &Pubkey,
    policy: &RelayPolicy,
    authorization: Option<&RelayAuthorization>,
) -> Result<Option<RelayAuthorizationClaims>, KoraError> {
    let relay = Pubkey::from_str(RELAY_PROGRAM_ID).map_err(|_| KoraError::ConfigError)?;
    let outer_count = transaction.transaction.message.instructions().len();
    let outer = transaction.all_instructions.get(..outer_count).ok_or_else(|| {
        KoraError::InvalidTransaction("Relay instructions are unresolved".to_string())
    })?;
    let has_relay = outer.iter().any(|instruction| instruction.program_id == relay);
    if !has_relay {
        if authorization.is_some() {
            return Err(KoraError::InvalidTransaction(
                "Relay authorization cannot authorize a non-Relay transaction".to_string(),
            ));
        }
        return Ok(None);
    }
    if !policy.enabled {
        return Err(KoraError::InvalidTransaction("Relay authorization is disabled".to_string()));
    }
    let authorization = authorization.ok_or_else(|| {
        KoraError::InvalidTransaction("Relay server authorization is required".to_string())
    })?;
    let payload = URL_SAFE_NO_PAD.decode(&authorization.payload).map_err(|_| {
        KoraError::InvalidTransaction("Relay authorization payload is invalid".to_string())
    })?;
    let verification_key =
        Pubkey::from_str(&policy.authorization_public_key).map_err(|_| KoraError::ConfigError)?;
    let signature = Signature::from_str(&authorization.signature).map_err(|_| {
        KoraError::InvalidTransaction("Relay authorization signature is invalid".to_string())
    })?;
    if !signature.verify(verification_key.as_ref(), &payload) {
        return Err(KoraError::InvalidTransaction(
            "Relay authorization signature verification failed".to_string(),
        ));
    }
    let claims: RelayAuthorizationClaims = serde_json::from_slice(&payload).map_err(|_| {
        KoraError::InvalidTransaction("Relay authorization claims are invalid".to_string())
    })?;
    validate_claims(transaction, payer, policy, outer, &claims)?;
    Ok(Some(claims))
}

fn validate_claims(
    transaction: &VersionedTransactionResolved,
    payer: &Pubkey,
    policy: &RelayPolicy,
    outer: &[Instruction],
    claims: &RelayAuthorizationClaims,
) -> Result<(), KoraError> {
    let message = match &transaction.transaction.message {
        VersionedMessage::V0(message) => message,
        _ => return Err(KoraError::InvalidTransaction("Relay requires a v0 message".to_string())),
    };
    let signer_keys = transaction.transaction.message.static_account_keys();
    if message.header.num_required_signatures != 2
        || message.header.num_readonly_signed_accounts != 0
        || transaction.transaction.signatures.len() != 2
        || transaction
            .transaction
            .signatures
            .iter()
            .any(|signature| *signature != Signature::default())
        || signer_keys.first() != Some(payer)
        || signer_keys.get(1).map(ToString::to_string).as_deref() != Some(claims.wallet.as_str())
        || claims.fee_payer != payer.to_string()
        || claims.wallet == claims.fee_payer
    {
        return Err(KoraError::InvalidTransaction(
            "Relay payer or user signer binding is invalid".to_string(),
        ));
    }
    if message.address_table_lookups.len() != 1
        || message.address_table_lookups[0].account_key.to_string() != RELAY_LOOKUP_TABLE
        || outer.len() != 1
        || outer[0].program_id.to_string() != RELAY_PROGRAM_ID
        || transaction
            .all_instructions
            .iter()
            .any(|instruction| instruction.accounts.iter().any(|account| account.pubkey == *payer))
        || outer[0].data.len() != 48
        || outer[0].accounts.get(1).map(|account| account.pubkey) != Some(signer_keys[1])
        || outer[0].accounts.get(2).map(|account| account.pubkey) != Some(signer_keys[1])
    {
        return Err(KoraError::InvalidTransaction("Relay message shape is invalid".to_string()));
    }
    let discriminator: [u8; 8] =
        outer[0].data[..8].try_into().map_err(|_| KoraError::ConfigError)?;
    let expected_accounts = if discriminator == NATIVE_DISCRIMINATOR {
        5
    } else if discriminator == TOKEN_DISCRIMINATOR {
        10
    } else {
        return Err(KoraError::InvalidTransaction(
            "Relay instruction discriminator is invalid".to_string(),
        ));
    };
    let expected_inner_program = if discriminator == NATIVE_DISCRIMINATOR {
        solana_system_interface::program::ID
    } else {
        spl_token_interface::id()
    };
    if transaction.all_instructions[outer.len()..]
        .iter()
        .any(|instruction| instruction.program_id != expected_inner_program)
    {
        return Err(KoraError::InvalidTransaction(
            "Relay inner program is not authorized for this deposit shape".to_string(),
        ));
    }
    let amount =
        u64::from_le_bytes(outer[0].data[8..16].try_into().map_err(|_| KoraError::ConfigError)?);
    let order_id = format!("0x{}", hex::encode(&outer[0].data[16..48]));
    let expected_mint = match claims.input_asset.as_str() {
        "SOL" => SYSTEM_PROGRAM_ID,
        "USDC" => USDC_MINT,
        "USDT" => USDT_MINT,
        _ => "",
    };
    let instruction_mint = if discriminator == NATIVE_DISCRIMINATOR {
        SYSTEM_PROGRAM_ID.to_string()
    } else {
        outer[0].accounts.get(4).map(|account| account.pubkey.to_string()).unwrap_or_default()
    };
    if outer[0].accounts.len() != expected_accounts
        || claims.schema_version != "relay-authorization-v1"
        || claims.action != "CROSS_CHAIN_RELAY"
        || claims.network != policy.authorization_network
        || claims.message_hash
            != hex::encode(Sha256::digest(transaction.transaction.message.serialize()))
        || claims.input_amount_raw.parse::<u64>().ok() != Some(amount)
        || claims.relay_order_id.to_lowercase() != order_id
        || claims.relay_request_id.is_empty()
        || claims.quote_id.is_empty()
        || claims.nonce.is_empty()
        || claims.destination_chain_id != 4663
        || !matches!(claims.input_asset.as_str(), "SOL" | "USDC" | "USDT")
        || claims.input_mint != expected_mint
        || instruction_mint != expected_mint
        || !matches!(claims.destination_asset.as_str(), "ETH" | "USDG")
        || !claims.recipient.starts_with("0x")
        || claims.recipient.len() != 42
        || hex::decode(&claims.recipient[2..]).map_or(true, |value| value.len() != 20)
        || claims.max_sponsor_lamports == 0
        || (claims.input_asset == "SOL") != (discriminator == NATIVE_DISCRIMINATOR)
    {
        return Err(KoraError::InvalidTransaction(
            "Relay authorization scope does not match the transaction".to_string(),
        ));
    }
    let now =
        SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| KoraError::ConfigError)?.as_secs();
    let lifetime =
        claims.expires_at_unix_seconds.checked_sub(claims.issued_at_unix_seconds).ok_or_else(
            || KoraError::InvalidTransaction("Relay authorization lifetime is invalid".to_string()),
        )?;
    if lifetime == 0
        || lifetime > policy.authorization_max_lifetime_seconds
        || claims.issued_at_unix_seconds > now.saturating_add(CLOCK_SKEW_SECONDS)
        || claims.expires_at_unix_seconds <= now
    {
        return Err(KoraError::InvalidTransaction(
            "Relay authorization is expired or outside its allowed lifetime".to_string(),
        ));
    }
    Ok(())
}

pub fn consume_relay_authorization(claims: &RelayAuthorizationClaims) -> Result<(), KoraError> {
    let now =
        SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| KoraError::ConfigError)?.as_secs();
    let mut used = USED_NONCES.lock().map_err(|_| {
        KoraError::InternalServerError("Relay authorization replay lock failed".to_string())
    })?;
    used.retain(|_, expires_at| *expires_at > now);
    if used.insert(claims.nonce.clone(), claims.expires_at_unix_seconds).is_some() {
        return Err(KoraError::InvalidTransaction(
            "Relay authorization was already used".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use solana_message::{
        compiled_instruction::CompiledInstruction,
        v0::{Message, MessageAddressTableLookup},
        MessageHeader, VersionedMessage,
    };
    use solana_sdk::{
        hash::Hash,
        signature::{Keypair, Signer},
        transaction::VersionedTransaction,
    };

    struct Fixture {
        authority: Keypair,
        payer: Keypair,
        wallet: Pubkey,
        policy: RelayPolicy,
        transaction: VersionedTransactionResolved,
    }

    fn fixture(token: bool) -> Fixture {
        let authority = Keypair::new();
        let payer = Keypair::new();
        let wallet = Pubkey::new_unique();
        let relay = Pubkey::from_str(RELAY_PROGRAM_ID).unwrap();
        let account_count = if token { 10 } else { 5 };
        let mut account_keys = vec![payer.pubkey(), wallet, relay];
        account_keys.extend((0..account_count).map(|_| Pubkey::new_unique()));
        if token {
            account_keys[7] = Pubkey::from_str(USDC_MINT).unwrap();
        }
        let mut data = Vec::from(if token { TOKEN_DISCRIMINATOR } else { NATIVE_DISCRIMINATOR });
        data.extend_from_slice(&500u64.to_le_bytes());
        data.extend_from_slice(&[7u8; 32]);
        let mut instruction_accounts: Vec<u8> = (3..3 + account_count as u8).collect();
        instruction_accounts[1] = 1;
        instruction_accounts[2] = 1;
        let message = Message {
            header: MessageHeader {
                num_required_signatures: 2,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            account_keys,
            recent_blockhash: Hash::new_unique(),
            instructions: vec![CompiledInstruction {
                program_id_index: 2,
                accounts: instruction_accounts,
                data,
            }],
            address_table_lookups: vec![MessageAddressTableLookup {
                account_key: Pubkey::from_str(RELAY_LOOKUP_TABLE).unwrap(),
                writable_indexes: vec![],
                readonly_indexes: vec![],
            }],
        };
        let transaction =
            VersionedTransactionResolved::from_kora_built_transaction(&VersionedTransaction {
                signatures: vec![Signature::default(), Signature::default()],
                message: VersionedMessage::V0(message),
            })
            .unwrap();
        let policy = RelayPolicy {
            enabled: true,
            authorization_public_key: authority.pubkey().to_string(),
            authorization_network: "mainnet-beta".to_string(),
            authorization_max_lifetime_seconds: 90,
        };
        Fixture { authority, payer, wallet, policy, transaction }
    }

    fn authorize(fixture: &Fixture, nonce: String, issued_at: u64) -> RelayAuthorization {
        let outer = &fixture.transaction.all_instructions[0];
        let claims = RelayAuthorizationClaims {
            schema_version: "relay-authorization-v1".to_string(),
            action: "CROSS_CHAIN_RELAY".to_string(),
            network: "mainnet-beta".to_string(),
            message_hash: hex::encode(Sha256::digest(
                fixture.transaction.transaction.message.serialize(),
            )),
            fee_payer: fixture.payer.pubkey().to_string(),
            wallet: fixture.wallet.to_string(),
            relay_order_id: format!("0x{}", hex::encode(&outer.data[16..48])),
            relay_request_id: Pubkey::new_unique().to_string(),
            quote_id: Pubkey::new_unique().to_string(),
            input_asset: if outer.accounts.len() == 5 { "SOL" } else { "USDC" }.to_string(),
            input_mint: if outer.accounts.len() == 5 { SYSTEM_PROGRAM_ID } else { USDC_MINT }
                .to_string(),
            input_amount_raw: "500".to_string(),
            destination_chain_id: 4663,
            destination_asset: "ETH".to_string(),
            recipient: "0x1111111111111111111111111111111111111111".to_string(),
            max_sponsor_lamports: 10_000,
            nonce,
            issued_at_unix_seconds: issued_at,
            expires_at_unix_seconds: issued_at + 60,
        };
        let payload = serde_json::to_vec(&claims).unwrap();
        RelayAuthorization {
            payload: URL_SAFE_NO_PAD.encode(&payload),
            signature: fixture.authority.sign_message(&payload).to_string(),
        }
    }

    fn now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    #[test]
    fn native_and_token_relay_messages_require_exact_authorization() {
        for fixture in [fixture(false), fixture(true)] {
            let authorization = authorize(&fixture, Pubkey::new_unique().to_string(), now());
            let claims = validate_relay_authorization(
                &fixture.transaction,
                &fixture.payer.pubkey(),
                &fixture.policy,
                Some(&authorization),
            )
            .unwrap();
            assert!(claims.is_some());
            let message = fixture.transaction.transaction.message.serialize();
            let mut signed = fixture.transaction.transaction.clone();
            signed.signatures[0] = fixture.payer.sign_message(&message);
            assert!(signed.signatures[0].verify(fixture.payer.pubkey().as_ref(), &message));
            assert_eq!(signed.message.serialize(), message);
            assert_eq!(signed.signatures[1], Signature::default());
            assert!(validate_relay_authorization(
                &fixture.transaction,
                &fixture.payer.pubkey(),
                &fixture.policy,
                None,
            )
            .is_err());
        }
    }

    #[test]
    fn every_message_mutation_invalidates_the_authorization() {
        let fixture = fixture(true);
        let authorization = authorize(&fixture, Pubkey::new_unique().to_string(), now());
        for mutation in 0..13 {
            let mut changed = fixture.transaction.clone();
            let VersionedMessage::V0(message) = &mut changed.transaction.message else {
                unreachable!()
            };
            match mutation {
                0 => message.instructions[0].data[8] ^= 1,
                1 => message.instructions[0].data[16] ^= 1,
                2 => message.instructions[0].accounts.swap(0, 1),
                3 => message.account_keys[3] = Pubkey::new_unique(),
                4 => message.address_table_lookups[0].account_key = Pubkey::new_unique(),
                5 => message.address_table_lookups[0].writable_indexes.push(0),
                6 => message.account_keys[0] = Pubkey::new_unique(),
                7 => message.account_keys[1] = Pubkey::new_unique(),
                8 => message.recent_blockhash = Hash::new_unique(),
                9 => message.instructions.push(message.instructions[0].clone()),
                10 => {
                    message.instructions.clear();
                }
                11 => message.header.num_readonly_signed_accounts = 1,
                12 => message.header.num_readonly_unsigned_accounts = 1,
                _ => unreachable!(),
            }
            assert!(validate_relay_authorization(
                &changed,
                &fixture.payer.pubkey(),
                &fixture.policy,
                Some(&authorization),
            )
            .is_err());
        }
    }

    #[test]
    fn authorization_rejects_expiry_wrong_scope_and_replay() {
        let fixture = fixture(false);
        let expired =
            authorize(&fixture, Pubkey::new_unique().to_string(), now().saturating_sub(120));
        assert!(validate_relay_authorization(
            &fixture.transaction,
            &fixture.payer.pubkey(),
            &fixture.policy,
            Some(&expired),
        )
        .is_err());

        let mut pre_signed = fixture.transaction.clone();
        pre_signed.transaction.signatures[1] =
            fixture.authority.sign_message(&pre_signed.transaction.message.serialize());
        let fresh = authorize(&fixture, Pubkey::new_unique().to_string(), now());
        assert!(validate_relay_authorization(
            &pre_signed,
            &fixture.payer.pubkey(),
            &fixture.policy,
            Some(&fresh),
        )
        .is_err());

        let authorization = authorize(&fixture, Pubkey::new_unique().to_string(), now());
        let claims = validate_relay_authorization(
            &fixture.transaction,
            &fixture.payer.pubkey(),
            &fixture.policy,
            Some(&authorization),
        )
        .unwrap()
        .unwrap();
        assert!(consume_relay_authorization(&claims).is_ok());
        assert!(consume_relay_authorization(&claims).is_err());

        let non_relay = fixture.transaction.clone();
        let mut policy = fixture.policy.clone();
        policy.enabled = false;
        assert!(validate_relay_authorization(
            &non_relay,
            &fixture.payer.pubkey(),
            &policy,
            Some(&authorization),
        )
        .is_err());
    }
}
