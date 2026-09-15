use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt,
    io::ErrorKind,
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use solana_signature::Signature;
use tokio::io::AsyncWriteExt;

use super::signatures::AddressCursor;

#[derive(Debug)]
pub(super) struct CheckpointMismatch(String);

impl fmt::Display for CheckpointMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CheckpointMismatch {}

const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const CHECKPOINT_WRITE_INTERVAL: Duration = Duration::from_secs(60);
const CHECKPOINT_ACTIVITY_WRITE_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(super) struct CheckpointStore {
    path: PathBuf,
    identity: CheckpointIdentity,
    replay_window_slots: u64,
    last_write_completed_at: Option<Instant>,
}

#[derive(Debug)]
pub(super) struct CheckpointSnapshot {
    pub(super) cursors: BTreeMap<String, AddressCursor>,
    pub(super) recent_signatures: BTreeMap<Signature, u64>,
    pub(super) quarantined_signatures: BTreeSet<Signature>,
}

#[derive(Debug)]
pub(super) enum CheckpointLoadOutcome {
    Missing,
    Loaded(CheckpointSnapshot),
    Corrupt {
        backup_path: PathBuf,
        reason: String,
    },
}

impl CheckpointStore {
    pub(super) fn new(
        path: PathBuf,
        genesis_hash: String,
        program_id: String,
        mut addresses: Vec<String>,
        replay_window_slots: u64,
    ) -> Self {
        addresses.sort();
        addresses.dedup();
        Self {
            path,
            identity: CheckpointIdentity {
                genesis_hash,
                program_id,
                addresses,
            },
            replay_window_slots,
            last_write_completed_at: None,
        }
    }

    pub(super) async fn quarantined_file_count(&self) -> anyhow::Result<u64> {
        let parent = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let prefix = match self.path.file_name().and_then(|name| name.to_str()) {
            Some(name) => format!("{name}.corrupt-"),
            None => return Ok(0),
        };
        let mut entries = match tokio::fs::read_dir(&parent).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reading RPC checkpoint directory {}", parent.display())
                })
            }
        };
        let mut count = 0;
        while let Some(entry) = entries
            .next_entry()
            .await
            .with_context(|| format!("reading RPC checkpoint directory {}", parent.display()))?
        {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
            {
                count += 1;
            }
        }
        Ok(count)
    }

    pub(super) async fn load(&self) -> anyhow::Result<CheckpointLoadOutcome> {
        let raw = match tokio::fs::read(&self.path).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(CheckpointLoadOutcome::Missing)
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading RPC checkpoint {}", self.path.display()))
            }
        };
        let document: CheckpointDocument = match serde_json::from_slice(&raw) {
            Ok(document) => document,
            Err(error) => {
                return self
                    .quarantine_corrupt(format!("checkpoint JSON is invalid: {error}"))
                    .await
            }
        };
        self.validate_identity(&document)?;

        let cursor_addresses = document.cursors.keys().cloned().collect::<Vec<_>>();
        if cursor_addresses != self.identity.addresses {
            return self
                .quarantine_corrupt("checkpoint has incomplete cursor data".to_string())
                .await;
        }

        let recent_signatures = match document
            .recent_signatures
            .into_iter()
            .map(|(encoded, slot)| {
                Signature::from_str(&encoded)
                    .map(|signature| (signature, slot))
                    .with_context(|| format!("checkpoint contains invalid signature {encoded}"))
            })
            .collect::<anyhow::Result<BTreeMap<_, _>>>()
        {
            Ok(signatures) => signatures,
            Err(error) => return self.quarantine_corrupt(format!("{error:#}")).await,
        };
        let cursors = document
            .cursors
            .into_iter()
            .map(|(address, scanned_slot)| {
                (
                    address,
                    AddressCursor {
                        scanned_slot: scanned_slot.saturating_sub(self.replay_window_slots),
                    },
                )
            })
            .collect();

        let quarantined_signatures = match document
            .quarantined_signatures
            .into_iter()
            .map(|encoded| {
                Signature::from_str(&encoded).with_context(|| {
                    format!("checkpoint contains invalid quarantined signature {encoded}")
                })
            })
            .collect::<anyhow::Result<BTreeSet<_>>>()
        {
            Ok(signatures) => signatures,
            Err(error) => return self.quarantine_corrupt(format!("{error:#}")).await,
        };

        Ok(CheckpointLoadOutcome::Loaded(CheckpointSnapshot {
            cursors,
            recent_signatures,
            quarantined_signatures,
        }))
    }

    pub(super) async fn save_if_due(
        &mut self,
        cursors: &BTreeMap<String, AddressCursor>,
        recent_signatures: &BTreeMap<Signature, u64>,
        quarantined_signatures: &BTreeSet<Signature>,
        activity: bool,
    ) -> anyhow::Result<bool> {
        // `load` rejects a partial cursor set as corrupt, so writing one before
        // the first poll installs them all quarantines a valid checkpoint.
        if cursors.keys().cloned().collect::<Vec<_>>() != self.identity.addresses {
            return Ok(false);
        }

        let interval = if activity {
            CHECKPOINT_ACTIVITY_WRITE_INTERVAL
        } else {
            CHECKPOINT_WRITE_INTERVAL
        };
        if self
            .last_write_completed_at
            .is_some_and(|saved| saved.elapsed() < interval)
        {
            return Ok(false);
        }

        let document = CheckpointDocument {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            genesis_hash: self.identity.genesis_hash.clone(),
            program_id: self.identity.program_id.clone(),
            addresses: self.identity.addresses.clone(),
            cursors: cursors
                .iter()
                .map(|(address, cursor)| (address.clone(), cursor.scanned_slot))
                .collect(),
            recent_signatures: recent_signatures
                .iter()
                .map(|(signature, slot)| (signature.to_string(), *slot))
                .collect(),
            quarantined_signatures: quarantined_signatures
                .iter()
                .map(ToString::to_string)
                .collect(),
        };
        let mut encoded =
            serde_json::to_vec_pretty(&document).context("serializing the RPC checkpoint")?;
        encoded.push(b'\n');

        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating RPC checkpoint directory {}", parent.display()))?;
        let temporary_path = temporary_path(&self.path);
        let mut temporary = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary_path)
            .await
            .with_context(|| {
                format!(
                    "opening temporary RPC checkpoint {}",
                    temporary_path.display()
                )
            })?;
        temporary
            .write_all(&encoded)
            .await
            .context("writing the RPC checkpoint")?;
        temporary
            .sync_all()
            .await
            .context("syncing the RPC checkpoint")?;
        drop(temporary);
        tokio::fs::rename(&temporary_path, &self.path)
            .await
            .with_context(|| {
                format!(
                    "renaming temporary RPC checkpoint {} to {}",
                    temporary_path.display(),
                    self.path.display()
                )
            })?;
        sync_directory(parent).await?;

        self.last_write_completed_at = Some(Instant::now());
        Ok(true)
    }

    fn validate_identity(&self, document: &CheckpointDocument) -> anyhow::Result<()> {
        let mismatch = |reason: String| Err(CheckpointMismatch(reason).into());
        if document.schema_version != CHECKPOINT_SCHEMA_VERSION {
            return mismatch(format!(
                "RPC checkpoint {} uses schema version {}, expected {}",
                self.path.display(),
                document.schema_version,
                CHECKPOINT_SCHEMA_VERSION
            ));
        }
        if document.genesis_hash != self.identity.genesis_hash {
            return mismatch(format!(
                "RPC checkpoint {} belongs to genesis hash {}, not {}",
                self.path.display(),
                document.genesis_hash,
                self.identity.genesis_hash
            ));
        }
        if document.program_id != self.identity.program_id {
            return mismatch(format!(
                "RPC checkpoint {} belongs to program {}, not {}",
                self.path.display(),
                document.program_id,
                self.identity.program_id
            ));
        }
        if document.addresses != self.identity.addresses {
            return mismatch(format!(
                "RPC checkpoint {} monitors different addresses; remove it before changing the deployment target",
                self.path.display()
            ));
        }
        Ok(())
    }

    async fn quarantine_corrupt(&self, reason: String) -> anyhow::Result<CheckpointLoadOutcome> {
        let backup_path = corrupt_path(&self.path);
        tokio::fs::rename(&self.path, &backup_path)
            .await
            .with_context(|| {
                format!(
                    "moving corrupt RPC checkpoint {} to {}",
                    self.path.display(),
                    backup_path.display()
                )
            })?;
        sync_directory(self.path.parent().unwrap_or_else(|| Path::new("."))).await?;
        Ok(CheckpointLoadOutcome::Corrupt {
            backup_path,
            reason,
        })
    }
}

#[derive(Debug)]
struct CheckpointIdentity {
    genesis_hash: String,
    program_id: String,
    addresses: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointDocument {
    schema_version: u32,
    genesis_hash: String,
    program_id: String,
    addresses: Vec<String>,
    cursors: BTreeMap<String, u64>,
    #[serde(default)]
    recent_signatures: BTreeMap<String, u64>,
    #[serde(default)]
    quarantined_signatures: BTreeSet<String>,
}

async fn sync_directory(path: &Path) -> anyhow::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all())
        .await
        .context("joining the RPC checkpoint directory sync")?
        .context("syncing the RPC checkpoint directory")
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut file_name = OsString::from(".");
    file_name.push(
        path.file_name()
            .expect("the RPC checkpoint path must include a file name"),
    );
    file_name.push(".tmp");
    path.with_file_name(file_name)
}

fn corrupt_path(path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock predates the unix epoch")
        .as_nanos();
    let mut file_name = path
        .file_name()
        .expect("the RPC checkpoint path must include a file name")
        .to_os_string();
    file_name.push(format!(".corrupt-{timestamp}"));
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
    };

    use serde_json::Value;
    use solana_pubkey::Pubkey;
    use solana_signature::Signature;

    use super::{CheckpointLoadOutcome, CheckpointStore};
    use crate::rpc_polling::signatures::AddressCursor;

    fn checkpoint_path() -> PathBuf {
        // Pubkey::new_unique restarts its sequence each process, so without the
        // run id a later run finds the previous run's files.
        std::env::temp_dir()
            .join(format!(
                "microscope-checkpoint-{}-{}",
                std::process::id(),
                Pubkey::new_unique()
            ))
            .join("rpc-polling.json")
    }

    fn signature(value: u64) -> Signature {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        Signature::from(bytes)
    }

    fn store(path: PathBuf, addresses: Vec<String>, replay_window_slots: u64) -> CheckpointStore {
        CheckpointStore::new(
            path,
            "genesis".to_string(),
            "program".to_string(),
            addresses,
            replay_window_slots,
        )
    }

    #[tokio::test]
    async fn saves_atomically_and_rewinds_loaded_cursors() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        let transaction = signature(1);
        let mut checkpoint = store(path.clone(), vec![address.clone()], 25);
        let cursors = BTreeMap::from([(address.clone(), AddressCursor { scanned_slot: 100 })]);
        let recent_signatures = BTreeMap::from([(transaction, 90)]);

        assert!(checkpoint
            .save_if_due(&cursors, &recent_signatures, &BTreeSet::new(), true)
            .await
            .unwrap());
        assert!(!path.with_file_name(".rpc-polling.json.tmp").exists());

        let reloaded = store(path.clone(), vec![address.clone()], 25);
        let CheckpointLoadOutcome::Loaded(loaded) = reloaded.load().await.unwrap() else {
            panic!("saved checkpoint should load");
        };
        assert_eq!(loaded.cursors[&address].scanned_slot, 75);
        assert_eq!(loaded.recent_signatures[&transaction], 90);

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn throttles_writes_during_sustained_activity() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        let cursors = BTreeMap::from([(address.clone(), AddressCursor { scanned_slot: 100 })]);
        let mut store = store(path.clone(), vec![address], 10);

        assert!(store
            .save_if_due(&cursors, &BTreeMap::new(), &BTreeSet::new(), true)
            .await
            .unwrap());
        assert!(!store
            .save_if_due(&cursors, &BTreeMap::new(), &BTreeSet::new(), true)
            .await
            .unwrap());

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn loading_does_not_delay_the_next_checkpoint_write() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        let cursors = BTreeMap::from([(address.clone(), AddressCursor { scanned_slot: 100 })]);
        let mut original = store(path.clone(), vec![address.clone()], 10);
        original
            .save_if_due(&cursors, &BTreeMap::new(), &BTreeSet::new(), false)
            .await
            .unwrap();

        let mut reloaded = store(path.clone(), vec![address], 10);
        assert!(matches!(
            reloaded.load().await.unwrap(),
            CheckpointLoadOutcome::Loaded(_)
        ));
        assert!(reloaded
            .save_if_due(&cursors, &BTreeMap::new(), &BTreeSet::new(), false)
            .await
            .unwrap());

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn rejects_checkpoints_from_another_cluster() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        let mut original = store(path.clone(), vec![address.clone()], 10);
        let cursors = BTreeMap::from([(address.clone(), AddressCursor { scanned_slot: 100 })]);
        original
            .save_if_due(&cursors, &BTreeMap::new(), &BTreeSet::new(), true)
            .await
            .unwrap();

        let wrong_cluster = CheckpointStore::new(
            path.clone(),
            "devnet".to_string(),
            "program".to_string(),
            vec![address],
            10,
        );
        let error = wrong_cluster.load().await.unwrap_err();
        assert!(error
            .to_string()
            .contains("belongs to genesis hash genesis"));
        assert!(
            error.is::<super::CheckpointMismatch>(),
            "identity mismatches must be typed so loading is not retried"
        );
        assert!(path.exists());

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn rejects_checkpoints_for_different_addresses() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        let mut original = store(path.clone(), vec![address.clone()], 10);
        let cursors = BTreeMap::from([(address, AddressCursor { scanned_slot: 100 })]);
        original
            .save_if_due(&cursors, &BTreeMap::new(), &BTreeSet::new(), true)
            .await
            .unwrap();

        let different_address = Pubkey::new_unique().to_string();
        let wrong_target = store(path.clone(), vec![different_address], 10);
        let error = wrong_target.load().await.unwrap_err();
        assert!(error.to_string().contains("monitors different addresses"));
        assert!(path.exists());

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn quarantines_invalid_json_and_starts_without_a_checkpoint() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not-json").unwrap();

        let checkpoint = store(path.clone(), vec![address], 10);
        let CheckpointLoadOutcome::Corrupt {
            backup_path,
            reason,
        } = checkpoint.load().await.unwrap()
        else {
            panic!("invalid JSON should be quarantined");
        };
        assert!(reason.contains("checkpoint JSON is invalid"));
        assert!(!path.exists());
        assert!(backup_path.exists());

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn quarantines_an_empty_checkpoint_instead_of_reporting_it_missing() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();

        let checkpoint = store(path.clone(), vec![address], 10);
        let CheckpointLoadOutcome::Corrupt { backup_path, .. } = checkpoint.load().await.unwrap()
        else {
            panic!("an empty checkpoint must not be mistaken for an absent one");
        };
        assert!(backup_path.exists());
        assert_eq!(
            checkpoint.quarantined_file_count().await.unwrap(),
            1,
            "the rebuilt cursor is only safe while the quarantine gauge alerts on it"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn quarantines_a_checkpoint_truncated_by_a_crashed_write() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        let mut original = store(path.clone(), vec![address.clone()], 10);
        let cursors = BTreeMap::from([(address.clone(), AddressCursor { scanned_slot: 100 })]);
        original
            .save_if_due(&cursors, &BTreeMap::new(), &BTreeSet::new(), true)
            .await
            .unwrap();
        let complete = std::fs::read(&path).unwrap();
        std::fs::write(&path, &complete[..complete.len() / 2]).unwrap();

        let reloaded = store(path.clone(), vec![address], 10);
        let CheckpointLoadOutcome::Corrupt { reason, .. } = reloaded.load().await.unwrap() else {
            panic!("a truncated checkpoint must not resume from its readable prefix");
        };
        assert!(reason.contains("checkpoint JSON is invalid"));
        assert!(!path.exists());

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn quarantines_incomplete_cursor_data() {
        let path = checkpoint_path();
        let first_address = Pubkey::new_unique().to_string();
        let second_address = Pubkey::new_unique().to_string();
        let addresses = vec![first_address.clone(), second_address.clone()];
        let mut original = store(path.clone(), addresses.clone(), 10);
        let cursors = BTreeMap::from([
            (first_address.clone(), AddressCursor { scanned_slot: 100 }),
            (second_address, AddressCursor { scanned_slot: 100 }),
        ]);
        original
            .save_if_due(&cursors, &BTreeMap::new(), &BTreeSet::new(), true)
            .await
            .unwrap();

        let mut document: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document["cursors"]
            .as_object_mut()
            .unwrap()
            .remove(&first_address);
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();

        let reloaded = store(path.clone(), addresses, 10);
        let CheckpointLoadOutcome::Corrupt {
            backup_path,
            reason,
        } = reloaded.load().await.unwrap()
        else {
            panic!("incomplete checkpoint should be quarantined");
        };
        assert!(reason.contains("incomplete cursor data"));
        assert!(!path.exists());
        assert!(backup_path.exists());

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn restores_quarantined_signatures() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        let quarantined = signature(9);
        let mut checkpoint = store(path.clone(), vec![address.clone()], 25);
        let cursors = BTreeMap::from([(address.clone(), AddressCursor { scanned_slot: 100 })]);

        assert!(checkpoint
            .save_if_due(
                &cursors,
                &BTreeMap::new(),
                &BTreeSet::from([quarantined]),
                true
            )
            .await
            .unwrap());

        let reloaded = store(path, vec![address], 25);
        let CheckpointLoadOutcome::Loaded(loaded) = reloaded.load().await.unwrap() else {
            panic!("saved checkpoint should load");
        };
        assert_eq!(loaded.quarantined_signatures, BTreeSet::from([quarantined]));
    }

    #[tokio::test]
    async fn loads_a_checkpoint_written_before_quarantine_was_persisted() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        let store_under_test = store(path.clone(), vec![address.clone()], 25);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &path,
            serde_json::json!({
                "schema_version": 1,
                "genesis_hash": "genesis",
                "program_id": "program",
                "addresses": [address.clone()],
                "cursors": { address.clone(): 100 },
                "recent_signatures": {},
            })
            .to_string(),
        )
        .await
        .unwrap();

        let CheckpointLoadOutcome::Loaded(loaded) = store_under_test.load().await.unwrap() else {
            panic!("legacy checkpoint should load");
        };
        assert!(loaded.quarantined_signatures.is_empty());
    }

    #[tokio::test]
    async fn does_not_write_before_every_address_has_a_cursor() {
        let path = checkpoint_path();
        let monitored = Pubkey::new_unique().to_string();
        let other = Pubkey::new_unique().to_string();
        let mut addresses = vec![monitored.clone(), other.clone()];
        addresses.sort();
        let mut checkpoint = store(path.clone(), addresses.clone(), 25);
        let partial = BTreeMap::from([(monitored, AddressCursor { scanned_slot: 100 })]);

        assert!(!checkpoint
            .save_if_due(&partial, &BTreeMap::new(), &BTreeSet::new(), true)
            .await
            .unwrap());
        assert!(!path.exists());

        let complete = addresses
            .iter()
            .map(|address| (address.clone(), AddressCursor { scanned_slot: 100 }))
            .collect();
        assert!(checkpoint
            .save_if_due(&complete, &BTreeMap::new(), &BTreeSet::new(), true)
            .await
            .unwrap());

        let reloaded = store(path, addresses, 25);
        assert!(matches!(
            reloaded.load().await.unwrap(),
            CheckpointLoadOutcome::Loaded(_)
        ));
    }

    #[tokio::test]
    async fn counts_quarantined_checkpoints_until_the_operator_removes_them() {
        let path = checkpoint_path();
        let address = Pubkey::new_unique().to_string();
        let store_under_test = store(path.clone(), vec![address], 25);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, "not json").await.unwrap();

        assert_eq!(store_under_test.quarantined_file_count().await.unwrap(), 0);
        let CheckpointLoadOutcome::Corrupt { backup_path, .. } =
            store_under_test.load().await.unwrap()
        else {
            panic!("invalid checkpoint should quarantine");
        };
        assert_eq!(store_under_test.quarantined_file_count().await.unwrap(), 1);

        tokio::fs::remove_file(backup_path).await.unwrap();
        assert_eq!(store_under_test.quarantined_file_count().await.unwrap(), 0);
    }
}
