#![cfg(test)]

use {
    super::*,
    crate::{
        accounts::{test_utils::create_test_accounts, Accounts},
        accounts_db::{get_temp_accounts_paths, AccountShrinkThreshold, AccountStorageMap},
        append_vec::AppendVec,
        bank::{Bank, Rewrites},
        epoch_accounts_hash,
        genesis_utils::{activate_all_features, activate_feature},
        snapshot_utils::ArchiveFormat,
        status_cache::StatusCache,
    },
    bincode::serialize_into,
    rand::{thread_rng, Rng},
    solana_sdk::{
        account::{AccountSharedData, ReadableAccount},
        clock::Slot,
        feature_set::disable_fee_calculator,
        genesis_config::{create_genesis_config, ClusterType},
        pubkey::Pubkey,
        signature::{Keypair, Signer},
    },
    std::{
        io::{BufReader, Cursor},
        path::Path,
        sync::{Arc, RwLock},
    },
    tempfile::TempDir,
};

/// Simulates the unpacking & storage reconstruction done during snapshot unpacking
fn copy_append_vecs<P: AsRef<Path>>(
    accounts_db: &AccountsDb,
    output_dir: P,
) -> std::io::Result<StorageAndNextAppendVecId> {
    let storage_entries = accounts_db
        .get_snapshot_storages(Slot::max_value(), None, None)
        .0;
    let storage: AccountStorageMap = AccountStorageMap::with_capacity(storage_entries.len());
    let mut next_append_vec_id = 0;
    for storage_entry in storage_entries.into_iter().flatten() {
        // Copy file to new directory
        let storage_path = storage_entry.get_path();
        let file_name = AppendVec::file_name(storage_entry.slot(), storage_entry.append_vec_id());
        let output_path = output_dir.as_ref().join(file_name);
        std::fs::copy(storage_path, &output_path)?;

        // Read new file into append-vec and build new entry
        let (append_vec, num_accounts) =
            AppendVec::new_from_file(output_path, storage_entry.accounts.len())?;
        let new_storage_entry = AccountStorageEntry::new_existing(
            storage_entry.slot(),
            storage_entry.append_vec_id(),
            append_vec,
            num_accounts,
        );
        next_append_vec_id = next_append_vec_id.max(new_storage_entry.append_vec_id());
        storage
            .entry(new_storage_entry.slot())
            .or_default()
            .write()
            .unwrap()
            .insert(
                new_storage_entry.append_vec_id(),
                Arc::new(new_storage_entry),
            );
    }

    Ok(StorageAndNextAppendVecId {
        storage,
        next_append_vec_id: AtomicAppendVecId::new(next_append_vec_id + 1),
    })
}

fn check_accounts(accounts: &Accounts, pubkeys: &[Pubkey], num: usize) {
    for _ in 1..num {
        let idx = thread_rng().gen_range(0, num - 1);
        let ancestors = vec![(0, 0)].into_iter().collect();
        let account = accounts.load_without_fixed_root(&ancestors, &pubkeys[idx]);
        let account1 = Some((
            AccountSharedData::new((idx + 1) as u64, 0, AccountSharedData::default().owner()),
            0,
        ));
        assert_eq!(account, account1);
    }
}

fn context_accountsdb_from_stream<'a, C, R>(
    stream: &mut BufReader<R>,
    account_paths: &[PathBuf],
    storage_and_next_append_vec_id: StorageAndNextAppendVecId,
) -> Result<AccountsDb, Error>
where
    C: TypeContext<'a>,
    R: Read,
{
    // read and deserialise the accounts database directly from the stream
    let accounts_db_fields = C::deserialize_accounts_db_fields(stream)?;
    let snapshot_accounts_db_fields = SnapshotAccountsDbFields {
        full_snapshot_accounts_db_fields: accounts_db_fields,
        incremental_snapshot_accounts_db_fields: None,
    };
    reconstruct_accountsdb_from_fields(
        snapshot_accounts_db_fields,
        account_paths,
        storage_and_next_append_vec_id,
        &GenesisConfig {
            cluster_type: ClusterType::Development,
            ..GenesisConfig::default()
        },
        AccountSecondaryIndexes::default(),
        false,
        None,
        AccountShrinkThreshold::default(),
        false,
        Some(crate::accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING),
        None,
        &Arc::default(),
        None,
    )
    .map(|(accounts_db, _)| accounts_db)
}

fn accountsdb_from_stream<R>(
    serde_style: SerdeStyle,
    stream: &mut BufReader<R>,
    account_paths: &[PathBuf],
    storage_and_next_append_vec_id: StorageAndNextAppendVecId,
) -> Result<AccountsDb, Error>
where
    R: Read,
{
    match serde_style {
        SerdeStyle::Newer => context_accountsdb_from_stream::<newer::Context, R>(
            stream,
            account_paths,
            storage_and_next_append_vec_id,
        ),
    }
}

fn accountsdb_to_stream<W>(
    serde_style: SerdeStyle,
    stream: &mut W,
    accounts_db: &AccountsDb,
    slot: Slot,
    account_storage_entries: &[SnapshotStorage],
) -> Result<(), Error>
where
    W: Write,
{
    match serde_style {
        SerdeStyle::Newer => serialize_into(
            stream,
            &SerializableAccountsDb::<newer::Context> {
                accounts_db,
                slot,
                account_storage_entries,
                phantom: std::marker::PhantomData::default(),
            },
        ),
    }
}

fn test_accounts_serialize_style(serde_style: SerdeStyle) {
    solana_logger::setup();
    let (_accounts_dir, paths) = get_temp_accounts_paths(4).unwrap();
    let accounts = Accounts::new_with_config_for_tests(
        paths,
        &ClusterType::Development,
        AccountSecondaryIndexes::default(),
        false,
        AccountShrinkThreshold::default(),
    );

    let mut pubkeys: Vec<Pubkey> = vec![];
    create_test_accounts(&accounts, &mut pubkeys, 100, 0);
    check_accounts(&accounts, &pubkeys, 100);
    accounts.add_root(0);

    let mut writer = Cursor::new(vec![]);
    accountsdb_to_stream(
        serde_style,
        &mut writer,
        &accounts.accounts_db,
        0,
        &accounts.accounts_db.get_snapshot_storages(0, None, None).0,
    )
    .unwrap();

    let copied_accounts = TempDir::new().unwrap();

    // Simulate obtaining a copy of the AppendVecs from a tarball
    let storage_and_next_append_vec_id =
        copy_append_vecs(&accounts.accounts_db, copied_accounts.path()).unwrap();

    let buf = writer.into_inner();
    let mut reader = BufReader::new(&buf[..]);
    let (_accounts_dir, daccounts_paths) = get_temp_accounts_paths(2).unwrap();
    let daccounts = Accounts::new_empty(
        accountsdb_from_stream(
            serde_style,
            &mut reader,
            &daccounts_paths,
            storage_and_next_append_vec_id,
        )
        .unwrap(),
    );
    check_accounts(&daccounts, &pubkeys, 100);
    assert_eq!(
        accounts.bank_hash_at(0, &Rewrites::default()),
        daccounts.bank_hash_at(0, &Rewrites::default())
    );
}

fn test_bank_serialize_style(
    serde_style: SerdeStyle,
    reserialize_accounts_hash: bool,
    update_accounts_hash: bool,
    incremental_snapshot_persistence: bool,
    initial_epoch_accounts_hash: bool,
) {
    solana_logger::setup();
    let (genesis_config, _) = create_genesis_config(500);
    let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
    let eah_start_slot = epoch_accounts_hash::calculation_start(&bank0);
    let bank1 = Bank::new_from_parent(&bank0, &Pubkey::default(), 1);
    bank0.squash();

    // Create an account on a non-root fork
    let key1 = Keypair::new();
    bank1.deposit(&key1.pubkey(), 5).unwrap();

    // If setting an initial EAH, then the bank being snapshotted must be in the EAH calculation
    // window.  Otherwise `bank_to_stream()` below will *not* include the EAH in the bank snapshot,
    // and the later-deserialized bank's EAH will not match the expected EAH.
    let bank2_slot = if initial_epoch_accounts_hash {
        eah_start_slot
    } else {
        0
    } + 2;
    let bank2 = Bank::new_from_parent(&bank0, &Pubkey::default(), bank2_slot);

    // Test new account
    let key2 = Keypair::new();
    bank2.deposit(&key2.pubkey(), 10).unwrap();
    assert_eq!(bank2.get_balance(&key2.pubkey()), 10);

    let key3 = Keypair::new();
    bank2.deposit(&key3.pubkey(), 0).unwrap();

    bank2.freeze();
    bank2.squash();
    bank2.force_flush_accounts_cache();

    let snapshot_storages = bank2.get_snapshot_storages(None);
    let mut buf = vec![];
    let mut writer = Cursor::new(&mut buf);

    let mut expected_epoch_accounts_hash = None;

    if initial_epoch_accounts_hash {
        expected_epoch_accounts_hash = Some(Hash::new(&[7; 32]));
        bank2
            .rc
            .accounts
            .accounts_db
            .epoch_accounts_hash_manager
            .set_valid(
                EpochAccountsHash::new(expected_epoch_accounts_hash.unwrap()),
                eah_start_slot,
            );
    }

    crate::serde_snapshot::bank_to_stream(
        serde_style,
        &mut std::io::BufWriter::new(&mut writer),
        &bank2,
        &snapshot_storages,
    )
    .unwrap();

    let accounts_hash = if update_accounts_hash {
        let hash = Hash::new(&[1; 32]);
        bank2
            .accounts()
            .accounts_db
            .set_accounts_hash(bank2.slot(), hash);
        hash
    } else {
        bank2.get_accounts_hash()
    };

    let slot = bank2.slot();
    let incremental =
        incremental_snapshot_persistence.then(|| BankIncrementalSnapshotPersistence {
            full_slot: slot + 1,
            full_hash: Hash::new(&[1; 32]),
            full_capitalization: 31,
            incremental_hash: Hash::new(&[2; 32]),
            incremental_capitalization: 32,
        });

    if reserialize_accounts_hash || incremental_snapshot_persistence {
        let temp_dir = TempDir::new().unwrap();
        let slot_dir = temp_dir.path().join(slot.to_string());
        let post_path = slot_dir.join(slot.to_string());
        let mut pre_path = post_path.clone();
        pre_path.set_extension(BANK_SNAPSHOT_PRE_FILENAME_EXTENSION);
        std::fs::create_dir(&slot_dir).unwrap();
        {
            let mut f = std::fs::File::create(&pre_path).unwrap();
            f.write_all(&buf).unwrap();
        }

        assert!(reserialize_bank_with_new_accounts_hash(
            temp_dir.path(),
            slot,
            &accounts_hash,
            incremental.as_ref(),
        ));
        let mut buf_reserialized;
        {
            let previous_len = buf.len();
            let expected = previous_len
                + if incremental_snapshot_persistence {
                    // previously saved a none (size = sizeof_None), now added a Some
                    let sizeof_none = std::mem::size_of::<u64>();
                    let sizeof_incremental_snapshot_persistence =
                        std::mem::size_of::<Option<BankIncrementalSnapshotPersistence>>();
                    sizeof_incremental_snapshot_persistence - sizeof_none
                } else {
                    // no change
                    0
                };

            // +1: larger buffer than expected to make sure the file isn't larger than expected
            buf_reserialized = vec![0; expected + 1];
            let mut f = std::fs::File::open(post_path).unwrap();
            let size = f.read(&mut buf_reserialized).unwrap();

            assert_eq!(
                size,
                expected,
                "(reserialize_accounts_hash, incremental_snapshot_persistence, update_accounts_hash, initial_epoch_accounts_hash): {:?}, previous_len: {previous_len}",
                (
                    reserialize_accounts_hash,
                    incremental_snapshot_persistence,
                    update_accounts_hash,
                    initial_epoch_accounts_hash,
                )
            );
            buf_reserialized.truncate(size);
        }
        if update_accounts_hash {
            // We cannot guarantee buffer contents are exactly the same if hash is the same.
            // Things like hashsets/maps have randomness in their in-mem representations.
            // This makes serialized bytes not deterministic.
            // But, we can guarantee that the buffer is different if we change the hash!
            assert_ne!(buf, buf_reserialized);
        }
        if update_accounts_hash || incremental_snapshot_persistence {
            buf = buf_reserialized;
        }
    }

    let rdr = Cursor::new(&buf[..]);
    let mut reader = std::io::BufReader::new(&buf[rdr.position() as usize..]);

    // Create a new set of directories for this bank's accounts
    let (_accounts_dir, dbank_paths) = get_temp_accounts_paths(4).unwrap();
    let mut status_cache = StatusCache::default();
    status_cache.add_root(2);
    // Create a directory to simulate AppendVecs unpackaged from a snapshot tar
    let copied_accounts = TempDir::new().unwrap();
    let storage_and_next_append_vec_id =
        copy_append_vecs(&bank2.rc.accounts.accounts_db, copied_accounts.path()).unwrap();
    let mut snapshot_streams = SnapshotStreams {
        full_snapshot_stream: &mut reader,
        incremental_snapshot_stream: None,
    };
    let mut dbank = crate::serde_snapshot::bank_from_streams(
        serde_style,
        &mut snapshot_streams,
        &dbank_paths,
        storage_and_next_append_vec_id,
        &genesis_config,
        &RuntimeConfig::default(),
        None,
        None,
        AccountSecondaryIndexes::default(),
        false,
        None,
        AccountShrinkThreshold::default(),
        false,
        Some(crate::accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING),
        None,
        &Arc::default(),
    )
    .unwrap();
    dbank.status_cache = Arc::new(RwLock::new(status_cache));
    assert_eq!(dbank.get_balance(&key1.pubkey()), 0);
    assert_eq!(dbank.get_balance(&key2.pubkey()), 10);
    assert_eq!(dbank.get_balance(&key3.pubkey()), 0);
    assert_eq!(dbank.get_accounts_hash(), accounts_hash);
    assert!(bank2 == dbank);
    assert_eq!(dbank.incremental_snapshot_persistence, incremental);
    assert_eq!(dbank.get_epoch_accounts_hash_to_serialize().map(|epoch_accounts_hash| *epoch_accounts_hash.as_ref()), expected_epoch_accounts_hash,
        "(reserialize_accounts_hash, incremental_snapshot_persistence, update_accounts_hash, initial_epoch_accounts_hash): {:?}",
        (
            reserialize_accounts_hash,
            incremental_snapshot_persistence,
            update_accounts_hash,
            initial_epoch_accounts_hash,
        )
    );
}

pub(crate) fn reconstruct_accounts_db_via_serialization(
    accounts: &AccountsDb,
    slot: Slot,
) -> AccountsDb {
    let mut writer = Cursor::new(vec![]);
    let snapshot_storages = accounts.get_snapshot_storages(slot, None, None).0;
    accountsdb_to_stream(
        SerdeStyle::Newer,
        &mut writer,
        accounts,
        slot,
        &snapshot_storages,
    )
    .unwrap();

    let buf = writer.into_inner();
    let mut reader = BufReader::new(&buf[..]);
    let copied_accounts = TempDir::new().unwrap();

    // Simulate obtaining a copy of the AppendVecs from a tarball
    let storage_and_next_append_vec_id =
        copy_append_vecs(accounts, copied_accounts.path()).unwrap();
    let mut accounts_db = accountsdb_from_stream(
        SerdeStyle::Newer,
        &mut reader,
        &[],
        storage_and_next_append_vec_id,
    )
    .unwrap();

    // The append vecs will be used from `copied_accounts` directly by the new AccountsDb so keep
    // its TempDir alive
    accounts_db
        .temp_paths
        .as_mut()
        .unwrap()
        .push(copied_accounts);

    accounts_db
}

#[test]
fn test_accounts_serialize_newer() {
    test_accounts_serialize_style(SerdeStyle::Newer)
}

#[test]
fn test_bank_serialize_newer() {
    for (reserialize_accounts_hash, update_accounts_hash) in
        [(false, false), (true, false), (true, true)]
    {
        let parameters = if reserialize_accounts_hash {
            [false, true].to_vec()
        } else {
            [false].to_vec()
        };
        for incremental_snapshot_persistence in parameters.clone() {
            for initial_epoch_accounts_hash in [false, true] {
                test_bank_serialize_style(
                    SerdeStyle::Newer,
                    reserialize_accounts_hash,
                    update_accounts_hash,
                    incremental_snapshot_persistence,
                    initial_epoch_accounts_hash,
                )
            }
        }
    }
}

#[test]
fn test_extra_fields_eof() {
    solana_logger::setup();
    let (mut genesis_config, _) = create_genesis_config(500);
    activate_feature(&mut genesis_config, disable_fee_calculator::id());

    let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
    bank0.squash();
    let mut bank = Bank::new_from_parent(&bank0, &Pubkey::default(), 1);

    // Set extra fields
    bank.fee_rate_governor.lamports_per_signature = 7000;

    // Serialize
    let snapshot_storages = bank.get_snapshot_storages(None);
    let mut buf = vec![];
    let mut writer = Cursor::new(&mut buf);
    crate::serde_snapshot::bank_to_stream(
        SerdeStyle::Newer,
        &mut std::io::BufWriter::new(&mut writer),
        &bank,
        &snapshot_storages,
    )
    .unwrap();

    // Deserialize
    let rdr = Cursor::new(&buf[..]);
    let mut reader = std::io::BufReader::new(&buf[rdr.position() as usize..]);
    let mut snapshot_streams = SnapshotStreams {
        full_snapshot_stream: &mut reader,
        incremental_snapshot_stream: None,
    };
    let (_accounts_dir, dbank_paths) = get_temp_accounts_paths(4).unwrap();
    let copied_accounts = TempDir::new().unwrap();
    let storage_and_next_append_vec_id =
        copy_append_vecs(&bank.rc.accounts.accounts_db, copied_accounts.path()).unwrap();
    let dbank = crate::serde_snapshot::bank_from_streams(
        SerdeStyle::Newer,
        &mut snapshot_streams,
        &dbank_paths,
        storage_and_next_append_vec_id,
        &genesis_config,
        &RuntimeConfig::default(),
        None,
        None,
        AccountSecondaryIndexes::default(),
        false,
        None,
        AccountShrinkThreshold::default(),
        false,
        Some(crate::accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING),
        None,
        &Arc::default(),
    )
    .unwrap();

    assert_eq!(
        bank.fee_rate_governor.lamports_per_signature,
        dbank.fee_rate_governor.lamports_per_signature
    );
}

#[test]
fn test_extra_fields_full_snapshot_archive() {
    solana_logger::setup();

    let (mut genesis_config, _) = create_genesis_config(500);
    activate_all_features(&mut genesis_config);

    let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
    let mut bank = Bank::new_from_parent(&bank0, &Pubkey::default(), 1);
    while !bank.is_complete() {
        bank.fill_bank_with_ticks_for_tests();
    }

    // Set extra field
    bank.fee_rate_governor.lamports_per_signature = 7000;

    let accounts_dir = TempDir::new().unwrap();
    let bank_snapshots_dir = TempDir::new().unwrap();
    let full_snapshot_archives_dir = TempDir::new().unwrap();
    let incremental_snapshot_archives_dir = TempDir::new().unwrap();

    // Serialize
    let snapshot_archive_info = snapshot_utils::bank_to_full_snapshot_archive(
        &bank_snapshots_dir,
        &bank,
        None,
        full_snapshot_archives_dir.path(),
        incremental_snapshot_archives_dir.path(),
        ArchiveFormat::TarBzip2,
        1,
        0,
    )
    .unwrap();

    // Deserialize
    let (dbank, _) = snapshot_utils::bank_from_snapshot_archives(
        &[PathBuf::from(accounts_dir.path())],
        bank_snapshots_dir.path(),
        &snapshot_archive_info,
        None,
        &genesis_config,
        &RuntimeConfig::default(),
        None,
        None,
        AccountSecondaryIndexes::default(),
        false,
        None,
        AccountShrinkThreshold::default(),
        false,
        false,
        false,
        Some(crate::accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING),
        None,
        &Arc::default(),
    )
    .unwrap();

    assert_eq!(
        bank.fee_rate_governor.lamports_per_signature,
        dbank.fee_rate_governor.lamports_per_signature
    );
}

#[test]
fn test_blank_extra_fields() {
    solana_logger::setup();
    let (mut genesis_config, _) = create_genesis_config(500);
    activate_feature(&mut genesis_config, disable_fee_calculator::id());

    let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
    bank0.squash();
    let mut bank = Bank::new_from_parent(&bank0, &Pubkey::default(), 1);

    // Set extra fields
    bank.fee_rate_governor.lamports_per_signature = 7000;

    // Serialize, but don't serialize the extra fields
    let snapshot_storages = bank.get_snapshot_storages(None);
    let mut buf = vec![];
    let mut writer = Cursor::new(&mut buf);
    crate::serde_snapshot::bank_to_stream_no_extra_fields(
        SerdeStyle::Newer,
        &mut std::io::BufWriter::new(&mut writer),
        &bank,
        &snapshot_storages,
    )
    .unwrap();

    // Deserialize
    let rdr = Cursor::new(&buf[..]);
    let mut reader = std::io::BufReader::new(&buf[rdr.position() as usize..]);
    let mut snapshot_streams = SnapshotStreams {
        full_snapshot_stream: &mut reader,
        incremental_snapshot_stream: None,
    };
    let (_accounts_dir, dbank_paths) = get_temp_accounts_paths(4).unwrap();
    let copied_accounts = TempDir::new().unwrap();
    let storage_and_next_append_vec_id =
        copy_append_vecs(&bank.rc.accounts.accounts_db, copied_accounts.path()).unwrap();
    let dbank = crate::serde_snapshot::bank_from_streams(
        SerdeStyle::Newer,
        &mut snapshot_streams,
        &dbank_paths,
        storage_and_next_append_vec_id,
        &genesis_config,
        &RuntimeConfig::default(),
        None,
        None,
        AccountSecondaryIndexes::default(),
        false,
        None,
        AccountShrinkThreshold::default(),
        false,
        Some(crate::accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING),
        None,
        &Arc::default(),
    )
    .unwrap();

    // Defaults to 0
    assert_eq!(0, dbank.fee_rate_governor.lamports_per_signature);
}

#[cfg(RUSTC_WITH_SPECIALIZATION)]
mod test_bank_serialize {
    use super::*;

    // This some what long test harness is required to freeze the ABI of
    // Bank's serialization due to versioned nature
    #[frozen_abi(digest = "C4asU4c7Qbd31QQDScqRPnT3iLCYc4qaGqeUQEGP7cTw")]
    #[derive(Serialize, AbiExample)]
    pub struct BankAbiTestWrapperNewer {
        #[serde(serialize_with = "wrapper_newer")]
        bank: Bank,
    }

    pub fn wrapper_newer<S>(bank: &Bank, s: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let snapshot_storages = bank
            .rc
            .accounts
            .accounts_db
            .get_snapshot_storages(0, None, None)
            .0;
        // ensure there is a single snapshot storage example for ABI digesting
        assert_eq!(snapshot_storages.len(), 1);

        (SerializableBankAndStorage::<newer::Context> {
            bank,
            snapshot_storages: &snapshot_storages,
            phantom: std::marker::PhantomData::default(),
        })
        .serialize(s)
    }
}

#[test]
fn test_reconstruct_historical_roots() {
    {
        let db = AccountsDb::default_for_tests();
        let historical_roots = vec![];
        let historical_roots_with_hash = vec![];
        reconstruct_historical_roots(&db, historical_roots, historical_roots_with_hash);
        let roots_tracker = db.accounts_index.roots_tracker.read().unwrap();
        assert!(roots_tracker.historical_roots.is_empty());
    }

    {
        let db = AccountsDb::default_for_tests();
        let historical_roots = vec![1];
        let historical_roots_with_hash = vec![(0, Hash::default())];
        reconstruct_historical_roots(&db, historical_roots, historical_roots_with_hash);
        let roots_tracker = db.accounts_index.roots_tracker.read().unwrap();
        assert_eq!(roots_tracker.historical_roots.get_all(), vec![0, 1]);
    }
    {
        let db = AccountsDb::default_for_tests();
        let historical_roots = vec![2, 1];
        let historical_roots_with_hash = vec![0, 5]
            .into_iter()
            .map(|slot| (slot, Hash::default()))
            .collect();
        reconstruct_historical_roots(&db, historical_roots, historical_roots_with_hash);
        let roots_tracker = db.accounts_index.roots_tracker.read().unwrap();
        assert_eq!(roots_tracker.historical_roots.get_all(), vec![0, 1, 2, 5]);
    }
}
