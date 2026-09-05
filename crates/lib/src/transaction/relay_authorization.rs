use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAuthorizationClaims {
    pub schema_version: String,
    pub action: String,
    pub network: String,
    pub message_hash: String,
    pub fee_payer: String,
    pub wallet: String,
    pub relay_order_id: String,
    pub relay_request_id: String,
    pub quote_id: String,
    pub input_asset: String,
    pub input_mint: String,
    pub input_amount_raw: String,
    pub destination_chain_id: u64,
    pub destination_asset: String,
    pub recipient: String,
    pub max_sponsor_lamports: u64,
    pub nonce: String,
    pub issued_at_unix_seconds: u64,
    pub expires_at_unix_seconds: u64,
}
