use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverAuthorizationClaims {
    pub schema_version: String,
    pub action: String,
    pub network: String,
    pub pilot_wallet: String,
    pub source_token_account: String,
    pub input_mint: String,
    pub input_amount_raw: String,
    pub output_mint: String,
    pub expected_output_lamports: String,
    pub minimum_output_lamports: String,
    pub minimum_user_payout_lamports: String,
    pub swap_fee_lamports: String,
    pub rent_fee_lamports: String,
    pub network_reimbursement_lamports: String,
    pub setup_rent_reimbursement_lamports: String,
    pub sponsored_cost_lamports: String,
    pub treasury: String,
    pub message_hash: String,
    pub quote_id: String,
    pub intent_id: String,
    pub nonce: String,
    pub issued_at_unix_seconds: u64,
    pub expires_at_unix_seconds: u64,
}
