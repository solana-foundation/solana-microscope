use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Mutex, OnceLock},
};

use anyhow::Context;
use carbon_core::{
    collection::InstructionDecoderCollection, error::CarbonResult, processor::Processor,
    transaction::TransactionProcessorInputType,
};
use serde::Serialize;
use solana_signature::Signature;

/// Records each transaction once its records are logged, so a restart cannot
/// replay it. The in-memory deduplication filter dies with the process and the
/// checkpoint is written on an interval, so anything shipped since the last
/// write returns through the poller's replay window as a duplicate.
static JOURNAL: OnceLock<Journal> = OnceLock::new();

struct Journal {
    path: PathBuf,
    state: Mutex<JournalState>,
}

#[derive(Default)]
struct JournalState {
    file: Option<File>,
    written: HashSet<Signature>,
    identity: Option<String>,
}

/// Names the cluster, program and addresses the entries below it belong to. A
/// journal that survives a failed removal cannot suppress another target's
/// transactions, because its header no longer matches.
const IDENTITY_PREFIX: &str = "identity ";

/// Only a running RPC poller reads and compacts the journal, so installing it
/// without one would grow a file nothing ever consumes.
pub(crate) fn install(path: PathBuf) {
    let _ = JOURNAL.set(Journal {
        path,
        state: Mutex::new(JournalState::default()),
    });
}

/// Carbon runs transaction pipes only after every instruction pipe has handled
/// the transaction, the one point where all of its records are logged.
/// Journalling from an instruction processor would let a restart suppress
/// records the later instructions never got to emit.
#[derive(Default)]
pub struct ShippedProcessor;

impl Processor<TransactionProcessorInputType<'_, NoInstructions>> for ShippedProcessor {
    async fn process(
        &mut self,
        input: &TransactionProcessorInputType<'_, NoInstructions>,
    ) -> CarbonResult<()> {
        record(input.metadata.signature, input.metadata.slot);
        Ok(())
    }
}

/// The pipe needs a decoder collection to route instructions through; this one
/// journals whole transactions and reads none of them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct NoInstructions;

impl InstructionDecoderCollection for NoInstructions {
    type InstructionType = ();

    fn parse_instruction(_instruction: &solana_instruction::Instruction) -> Option<Self> {
        None
    }

    fn get_type(&self) -> Self::InstructionType {}
}

fn record(signature: Signature, slot: u64) {
    let Some(journal) = JOURNAL.get() else {
        return;
    };
    let mut state = journal
        .state
        .lock()
        .expect("the shipped journal is poisoned");
    if !state.written.insert(signature) {
        return;
    }
    if let Err(error) = journal.append(&mut state, signature, slot) {
        state.written.remove(&signature);
        log::error!("failed to journal shipped transaction {signature}: {error:#}");
    }
}

pub(crate) fn restore(identity: String) -> Vec<(Signature, u64)> {
    let Some(journal) = JOURNAL.get() else {
        return Vec::new();
    };
    let mut state = journal
        .state
        .lock()
        .expect("the shipped journal is poisoned");
    let entries = match read_journal(&journal.path) {
        Ok((header, entries)) if header.as_deref() == Some(identity.as_str()) => entries,
        Ok((header, _)) => {
            if header.is_some() {
                log::warn!(
                    "the shipped-transaction journal {} belongs to another deployment target and will not suppress anything",
                    journal.path.display()
                );
            }
            let _ = journal.purge(&mut state);
            Vec::new()
        }
        Err(error) => {
            log::error!(
                "failed to read the shipped-transaction journal {}, so transactions shipped since the last checkpoint may be re-emitted: {error:#}",
                journal.path.display()
            );
            Vec::new()
        }
    };
    state.written = entries.iter().map(|(signature, _)| *signature).collect();
    state.identity = Some(identity);
    entries
}

/// Drops a journal that has no checkpoint to vouch for it: entries with no
/// durable progress behind them suppress nothing a restart can rediscover.
pub(crate) fn discard(identity: String) {
    let Some(journal) = JOURNAL.get() else {
        return;
    };
    let mut state = journal
        .state
        .lock()
        .expect("the shipped journal is poisoned");
    let purged = journal.purge(&mut state);
    state.identity = Some(identity);
    match purged {
        Ok(true) => log::warn!(
            "cleared the shipped-transaction journal {}, which has no checkpoint to establish what deployment it belongs to",
            journal.path.display()
        ),
        Ok(false) => {}
        Err(error) => log::error!(
            "failed to remove the shipped-transaction journal {}: {error:#}",
            journal.path.display()
        ),
    }
}

/// Mirrors the checkpoint's recent-signature retention: below the replay floor
/// the poller cannot rediscover the transaction, so the entry suppresses nothing.
pub(crate) fn compact(minimum_slot: u64) {
    let Some(journal) = JOURNAL.get() else {
        return;
    };
    let mut state = journal
        .state
        .lock()
        .expect("the shipped journal is poisoned");
    if let Err(error) = journal.rewrite(&mut state, minimum_slot) {
        log::warn!(
            "failed to compact the shipped-transaction journal {}: {error:#}",
            journal.path.display()
        );
    }
}

impl Journal {
    fn append(
        &self,
        state: &mut JournalState,
        signature: Signature,
        slot: u64,
    ) -> anyhow::Result<()> {
        if state.file.is_none() {
            let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating shipped-journal directory {}", parent.display())
            })?;
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .with_context(|| format!("opening shipped journal {}", self.path.display()))?;
            let empty = file
                .metadata()
                .with_context(|| format!("reading shipped journal {}", self.path.display()))?
                .len()
                == 0;
            if empty {
                if let Some(identity) = &state.identity {
                    file.write_all(format!("{IDENTITY_PREFIX}{identity}\n").as_bytes())
                        .with_context(|| {
                            format!("stamping shipped journal {}", self.path.display())
                        })?;
                }
            }
            state.file = Some(file);
        }
        // Unbuffered, unsynced: the write reaches the page cache, which is what
        // a killed process needs. Only a host crash loses it, and that degrades
        // to the duplicates this journal exists to avoid, never to loss.
        state
            .file
            .as_mut()
            .expect("the journal file was just opened")
            .write_all(format!("{signature} {slot}\n").as_bytes())
            .with_context(|| format!("appending to shipped journal {}", self.path.display()))
    }

    fn purge(&self, state: &mut JournalState) -> anyhow::Result<bool> {
        state.file = None;
        state.written.clear();
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            // Removal needs directory write permission, truncation only the
            // file's; an entry left behind would resurface on the next restart
            // and suppress another target's transactions.
            Err(remove_error) => match File::create(&self.path) {
                Ok(_) => Ok(true),
                Err(_) => Err(remove_error)
                    .with_context(|| format!("removing shipped journal {}", self.path.display())),
            },
        }
    }

    fn rewrite(&self, state: &mut JournalState, minimum_slot: u64) -> anyhow::Result<()> {
        let (header, entries) = read_journal(&self.path)?;
        let retained = entries
            .iter()
            .filter(|(_, slot)| *slot > minimum_slot)
            .collect::<Vec<_>>();
        if retained.len() == entries.len() {
            return Ok(());
        }

        let temporary_path = temporary_path(&self.path);
        let mut temporary = File::create(&temporary_path).with_context(|| {
            format!(
                "creating temporary shipped journal {}",
                temporary_path.display()
            )
        })?;
        if let Some(header) = header {
            temporary
                .write_all(format!("{IDENTITY_PREFIX}{header}\n").as_bytes())
                .context("writing the compacted shipped journal")?;
        }
        for (signature, slot) in &retained {
            temporary
                .write_all(format!("{signature} {slot}\n").as_bytes())
                .context("writing the compacted shipped journal")?;
        }
        drop(temporary);
        std::fs::rename(&temporary_path, &self.path).with_context(|| {
            format!(
                "renaming temporary shipped journal {} to {}",
                temporary_path.display(),
                self.path.display()
            )
        })?;

        state.file = None;
        state.written = retained.iter().map(|(signature, _)| *signature).collect();
        Ok(())
    }
}

type JournalContents = (Option<String>, Vec<(Signature, u64)>);

fn read_journal(path: &Path) -> anyhow::Result<JournalContents> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((None, Vec::new())),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading shipped journal {}", path.display()))
        }
    };
    let mut lines = raw.lines().peekable();
    let header = lines
        .peek()
        .and_then(|line| line.strip_prefix(IDENTITY_PREFIX))
        .map(ToOwned::to_owned);
    if header.is_some() {
        lines.next();
    }
    let mut entries = Vec::new();
    let mut skipped = 0;
    for line in lines {
        match parse_entry(line) {
            Some(entry) => entries.push(entry),
            // A kill mid-write leaves a partial trailing line; dropping it only
            // costs the duplicate suppression for that one transaction.
            None if !line.is_empty() => skipped += 1,
            None => {}
        }
    }
    if skipped > 0 {
        log::warn!(
            "skipped {skipped} unreadable shipped-journal entry(s) in {}",
            path.display()
        );
    }
    Ok((header, entries))
}

fn parse_entry(line: &str) -> Option<(Signature, u64)> {
    let (signature, slot) = line.split_once(' ')?;
    Some((Signature::from_str(signature).ok()?, slot.parse().ok()?))
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut file_name = OsString::from(".");
    file_name.push(
        path.file_name()
            .expect("the shipped journal path must include a file name"),
    );
    file_name.push(".tmp");
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use solana_pubkey::Pubkey;
    use solana_signature::Signature;

    use super::{read_journal, temporary_path, Journal, JournalState};

    fn entries(path: &Path) -> Vec<(Signature, u64)> {
        read_journal(path).unwrap().1
    }

    fn signature(value: u64) -> Signature {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        Signature::from(bytes)
    }

    fn journal() -> (Journal, PathBuf) {
        let path = std::env::temp_dir()
            .join(format!(
                "microscope-shipped-{}-{}",
                std::process::id(),
                Pubkey::new_unique()
            ))
            .join("shipped.log");
        (
            Journal {
                path: path.clone(),
                state: std::sync::Mutex::new(JournalState::default()),
            },
            path,
        )
    }

    #[test]
    fn replays_every_shipped_transaction_after_a_restart() {
        let (journal, path) = journal();
        let mut state = JournalState::default();

        journal.append(&mut state, signature(1), 100).unwrap();
        journal.append(&mut state, signature(2), 101).unwrap();

        assert_eq!(
            entries(&path),
            vec![(signature(1), 100), (signature(2), 101)]
        );

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// A kill between the write and the newline must not cost the entries
    /// before it, which are the ones that would otherwise be re-emitted.
    #[test]
    fn keeps_the_entries_before_a_torn_trailing_line() {
        let (journal, path) = journal();
        let mut state = JournalState::default();
        journal.append(&mut state, signature(1), 100).unwrap();
        std::fs::write(
            &path,
            format!("{} 100\n{}", signature(1), &signature(2).to_string()[..20]),
        )
        .unwrap();

        assert_eq!(entries(&path), vec![(signature(1), 100)]);

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn compaction_drops_entries_below_the_replay_floor() {
        let (journal, path) = journal();
        let mut state = JournalState::default();
        journal.append(&mut state, signature(1), 90).unwrap();
        journal.append(&mut state, signature(2), 101).unwrap();

        journal.rewrite(&mut state, 100).unwrap();

        assert_eq!(entries(&path), vec![(signature(2), 101)]);
        assert!(!temporary_path(&path).exists());
        // A dropped entry must be re-journalable, or a rediscovered
        // transaction is silently never recorded again.
        assert!(!state.written.contains(&signature(1)));
        assert!(state.written.contains(&signature(2)));

        journal.append(&mut state, signature(3), 102).unwrap();
        assert_eq!(
            entries(&path),
            vec![(signature(2), 101), (signature(3), 102)]
        );

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn compaction_leaves_a_journal_entirely_above_the_floor_alone() {
        let (journal, path) = journal();
        let mut state = JournalState::default();
        journal.append(&mut state, signature(1), 101).unwrap();

        journal.rewrite(&mut state, 100).unwrap();

        assert!(
            state.file.is_some(),
            "an untouched journal keeps its handle"
        );
        assert_eq!(entries(&path), vec![(signature(1), 101)]);

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// A journal kept across a change of deployment target would suppress the
    /// new target's transactions, and nothing in the file itself says which
    /// cluster or program it belongs to.
    #[test]
    fn purging_leaves_nothing_to_suppress_a_new_target() {
        let (journal, path) = journal();
        let mut state = JournalState::default();
        journal.append(&mut state, signature(1), 100).unwrap();

        assert!(journal.purge(&mut state).unwrap());

        assert!(!path.exists());
        assert!(state.written.is_empty());
        assert!(entries(&path).is_empty());
        // Purging must not wedge the journal for the run that follows it.
        journal.append(&mut state, signature(2), 200).unwrap();
        assert_eq!(entries(&path), vec![(signature(2), 200)]);

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// Removal fails without directory write permission; a journal left behind
    /// there would resurface on the next restart against another target.
    #[cfg(unix)]
    #[test]
    fn purging_truncates_when_the_journal_cannot_be_removed() {
        use std::os::unix::fs::PermissionsExt;

        let (journal, path) = journal();
        let mut state = JournalState::default();
        journal.append(&mut state, signature(1), 100).unwrap();
        let directory = path.parent().unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o555)).unwrap();

        let purged = journal.purge(&mut state);
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(purged.unwrap());
        assert!(entries(&path).is_empty());

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The header is what a surviving journal is judged on, so it has to reach
    /// the file before any entry does and outlive compaction.
    #[test]
    fn a_stamped_journal_keeps_its_identity_through_compaction() {
        let (journal, path) = journal();
        let mut state = JournalState {
            identity: Some("mainnet program address".to_owned()),
            ..JournalState::default()
        };
        journal.append(&mut state, signature(1), 90).unwrap();
        journal.append(&mut state, signature(2), 101).unwrap();

        journal.rewrite(&mut state, 100).unwrap();

        let (header, retained) = read_journal(&path).unwrap();
        assert_eq!(header.as_deref(), Some("mainnet program address"));
        assert_eq!(retained, vec![(signature(2), 101)]);

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn purging_a_journal_that_was_never_written_is_not_an_error() {
        let (journal, _) = journal();

        assert!(!journal.purge(&mut JournalState::default()).unwrap());
    }

    #[test]
    fn a_missing_journal_restores_nothing() {
        let (_, path) = journal();

        assert!(entries(&path).is_empty());
    }
}
