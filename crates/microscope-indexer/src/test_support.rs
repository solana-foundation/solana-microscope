use std::str::FromStr;

use solana_signature::Signature;
use solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta;

use crate::config::Config;

pub fn signature(value: u64) -> Signature {
    let mut bytes = [0; 64];
    bytes[..8].copy_from_slice(&value.to_le_bytes());
    Signature::from(bytes)
}

pub fn config() -> Config {
    Config::parse(
        r#"
program_id = "11111111111111111111111111111111"
idl_path = "Cargo.toml"

[multisig]
vault_address = "11111111111111111111111111111111"
state_address = "11111111111111111111111111111111"
version = "v4"
"#,
    )
    .expect("test config parses")
}

pub fn squads_v4_vault_execute() -> (Signature, EncodedConfirmedTransactionWithStatusMeta) {
    let transaction = serde_json::from_str(include_str!(
        "../tests/fixtures/squads_v4_vault_execute.json"
    ))
    .expect("fixture deserializes");
    let signature = Signature::from_str(
        "DTJvwK9o6DjaUZs5NF598Qbhk89uahfXyorkXUWhhr8iH5x3QHbQAGum97LEUqzC7LiJ8FYeK19P2vJMKVo74DS",
    )
    .unwrap();
    (signature, transaction)
}
