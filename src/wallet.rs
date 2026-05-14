use crate::config::BoxError;
use solana_sdk::signature::Keypair;
use std::path::Path;

pub fn load(path: impl AsRef<Path>) -> Result<Keypair, BoxError> {
    let raw = std::fs::read_to_string(path.as_ref())
        .map_err(|e| format!("read wallet {}: {e}", path.as_ref().display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&raw)
        .map_err(|e| format!("wallet not in Solana CLI keypair format (JSON byte array): {e}"))?;
    let kp = Keypair::from_bytes(&bytes)
        .map_err(|e| format!("invalid keypair bytes: {e}"))?;
    Ok(kp)
}
