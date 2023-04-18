use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::fs;
use std::io::prelude::*;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use ark_std::rand::prelude::*;
use ark_std::rand::SeedableRng;
use bimap::BiHashMap;
use bip39::Seed;
use file_lock::{FileLock, FileOptions};
use itertools::Itertools;
use masp_primitives::zip32::ExtendedFullViewingKey;
use namada::types::address::{Address, ImplicitAddress};
use namada::types::key::dkg_session_keys::DkgKeypair;
use namada::types::key::*;
use namada::types::masp::{
    ExtendedSpendingKey, ExtendedViewingKey, PaymentAddress,
};
use namada::types::transaction::EllipticCurve;
use serde::{Deserialize, Serialize};
use slip10_ed25519;
use thiserror::Error;

use super::alias::{self, Alias};
use super::derivation_path::DerivationPath;
use super::keys::StoredKeypair;
use super::pre_genesis;
use crate::cli;
use crate::config::genesis::genesis_config::GenesisConfig;

/// Special keys for a validator
#[derive(Serialize, Deserialize, Debug)]
pub struct ValidatorKeys {
    /// Special keypair for signing protocol txs
    pub protocol_keypair: common::SecretKey,
    /// Special session keypair needed by validators for participating
    /// in the DKG protocol
    pub dkg_keypair: Option<DkgKeypair>,
}

impl ValidatorKeys {
    /// Get the protocol keypair
    pub fn get_protocol_keypair(&self) -> &common::SecretKey {
        &self.protocol_keypair
    }
}

/// Special data associated with a validator
#[derive(Serialize, Deserialize, Debug)]
pub struct ValidatorData {
    /// The address associated to a validator
    pub address: Address,
    /// special keys for a validator
    pub keys: ValidatorKeys,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Store {
    /// Known viewing keys
    view_keys: HashMap<Alias, ExtendedViewingKey>,
    /// Known spending keys
    spend_keys: HashMap<Alias, StoredKeypair<ExtendedSpendingKey>>,
    /// Known payment addresses
    payment_addrs: HashMap<Alias, PaymentAddress>,
    /// Cryptographic keypairs
    keys: HashMap<Alias, StoredKeypair<common::SecretKey>>,
    /// Namada address book
    addresses: BiHashMap<Alias, Address>,
    /// Known mappings of public key hashes to their aliases in the `keys`
    /// field. Used for look-up by a public key.
    pkhs: HashMap<PublicKeyHash, Alias>,
    /// Special keys if the wallet belongs to a validator
    pub(crate) validator_data: Option<ValidatorData>,
    /// Namada address vp type
    address_vp_types: HashMap<AddressVpType, HashSet<Address>>,
}

/// Grouping of addresses by validity predicate.
#[derive(Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash, Debug)]
pub enum AddressVpType {
    Token,
}

#[derive(Error, Debug)]
pub enum LoadStoreError {
    #[error("Failed decoding the wallet store: {0}")]
    Decode(toml::de::Error),
    #[error("Failed to read the wallet store from {0}: {1}")]
    ReadWallet(String, String),
    #[error("Failed to write the wallet store: {0}")]
    StoreNewWallet(String),
}

impl Store {
    #[cfg(not(feature = "dev"))]
    fn new(genesis: GenesisConfig) -> Self {
        let mut store = Self::default();
        store.add_genesis_addresses(genesis);
        store
    }

    #[cfg(feature = "dev")]
    fn new() -> Self {
        let mut store = Self::default();
        // Pre-load the default keys without encryption
        let no_password = None;
        for (alias, keypair) in super::defaults::keys() {
            let pkh: PublicKeyHash = (&keypair.ref_to()).into();
            store.keys.insert(
                alias.clone(),
                StoredKeypair::new(keypair, no_password.clone()).0,
            );
            store.pkhs.insert(pkh, alias);
        }
        store
            .addresses
            .extend(super::defaults::addresses().into_iter());
        store
    }

    /// Add addresses from a genesis configuration.
    pub fn add_genesis_addresses(&mut self, genesis: GenesisConfig) {
        for (alias, token) in &genesis.token {
            if let Some(address) = token.address.as_ref() {
                match Address::from_str(address) {
                    Ok(address) => self
                        .add_vp_type_to_address(AddressVpType::Token, address),
                    Err(_) => {
                        tracing::error!(
                            "Weird address for token {alias}: {address}"
                        )
                    }
                }
            }
        }
        self.addresses.extend(
            super::defaults::addresses_from_genesis(genesis).into_iter(),
        );
    }

    /// Save the wallet store to a file.
    pub fn save(&self, store_dir: &Path) -> std::io::Result<()> {
        let data = self.encode();
        let wallet_path = wallet_file(store_dir);
        // Make sure the dir exists
        let wallet_dir = wallet_path.parent().unwrap();
        fs::create_dir_all(wallet_dir)?;
        // Write the file
        let options =
            FileOptions::new().create(true).write(true).truncate(true);
        let mut filelock =
            FileLock::lock(wallet_path.to_str().unwrap(), true, options)?;
        filelock.file.write_all(&data)
    }

    /// Load the store file or create a new one without any keys or addresses.
    pub fn load_or_new(store_dir: &Path) -> Result<Self, LoadStoreError> {
        Self::load(store_dir).or_else(|_| {
            let store = Self::default();
            store.save(store_dir).map_err(|err| {
                LoadStoreError::StoreNewWallet(err.to_string())
            })?;
            Ok(store)
        })
    }

    /// Load the store file or create a new one with the default addresses from
    /// the genesis file, if not found.
    pub fn load_or_new_from_genesis(
        store_dir: &Path,
        genesis_cfg: GenesisConfig,
    ) -> Result<Self, LoadStoreError> {
        Self::load(store_dir).or_else(|_| {
            #[cfg(not(feature = "dev"))]
            let store = Self::new(genesis_cfg);
            #[cfg(feature = "dev")]
            let store = {
                // The function is unused in dev
                let _ = genesis_cfg;
                Self::new()
            };
            store.save(store_dir).map_err(|err| {
                LoadStoreError::StoreNewWallet(err.to_string())
            })?;
            Ok(store)
        })
    }

    /// Attempt to load the store file.
    pub fn load(store_dir: &Path) -> Result<Self, LoadStoreError> {
        let wallet_file = wallet_file(store_dir);
        match FileLock::lock(
            wallet_file.to_str().unwrap(),
            true,
            FileOptions::new().read(true).write(false),
        ) {
            Ok(mut filelock) => {
                let mut store = Vec::<u8>::new();
                filelock.file.read_to_end(&mut store).map_err(|err| {
                    LoadStoreError::ReadWallet(
                        store_dir.to_str().unwrap().into(),
                        err.to_string(),
                    )
                })?;
                Store::decode(store).map_err(LoadStoreError::Decode)
            }
            Err(err) => Err(LoadStoreError::ReadWallet(
                wallet_file.to_string_lossy().into_owned(),
                err.to_string(),
            )),
        }
    }

    /// Find the stored key by an alias, a public key hash or a public key.
    pub fn find_key(
        &self,
        alias_pkh_or_pk: impl AsRef<str>,
    ) -> Option<&StoredKeypair<common::SecretKey>> {
        let alias_pkh_or_pk = alias_pkh_or_pk.as_ref();
        // Try to find by alias
        self.keys
            .get(&alias_pkh_or_pk.into())
            // Try to find by PKH
            .or_else(|| {
                let pkh = PublicKeyHash::from_str(alias_pkh_or_pk).ok()?;
                self.find_key_by_pkh(&pkh)
            })
            // Try to find by PK
            .or_else(|| {
                let pk = common::PublicKey::from_str(alias_pkh_or_pk).ok()?;
                self.find_key_by_pk(&pk)
            })
    }

    pub fn find_spending_key(
        &self,
        alias: impl AsRef<str>,
    ) -> Option<&StoredKeypair<ExtendedSpendingKey>> {
        self.spend_keys.get(&alias.into())
    }

    pub fn find_viewing_key(
        &self,
        alias: impl AsRef<str>,
    ) -> Option<&ExtendedViewingKey> {
        self.view_keys.get(&alias.into())
    }

    pub fn find_payment_addr(
        &self,
        alias: impl AsRef<str>,
    ) -> Option<&PaymentAddress> {
        self.payment_addrs.get(&alias.into())
    }

    /// Find the stored key by a public key.
    pub fn find_key_by_pk(
        &self,
        pk: &common::PublicKey,
    ) -> Option<&StoredKeypair<common::SecretKey>> {
        let pkh = PublicKeyHash::from(pk);
        self.find_key_by_pkh(&pkh)
    }

    /// Find the stored key by a public key hash.
    pub fn find_key_by_pkh(
        &self,
        pkh: &PublicKeyHash,
    ) -> Option<&StoredKeypair<common::SecretKey>> {
        let alias = self.pkhs.get(pkh)?;
        self.keys.get(alias)
    }

    /// Find the stored alias for a public key hash.
    pub fn find_alias_by_pkh(&self, pkh: &PublicKeyHash) -> Option<Alias> {
        self.pkhs.get(pkh).cloned()
    }

    /// Find the stored address by an alias.
    pub fn find_address(&self, alias: impl AsRef<str>) -> Option<&Address> {
        self.addresses.get_by_left(&alias.into())
    }

    /// Find an alias by the address if it's in the wallet.
    pub fn find_alias(&self, address: &Address) -> Option<&Alias> {
        self.addresses.get_by_right(address)
    }

    /// Get all known keys by their alias, paired with PKH, if known.
    pub fn get_keys(
        &self,
    ) -> HashMap<
        Alias,
        (&StoredKeypair<common::SecretKey>, Option<&PublicKeyHash>),
    > {
        let mut keys: HashMap<
            Alias,
            (&StoredKeypair<common::SecretKey>, Option<&PublicKeyHash>),
        > = self
            .pkhs
            .iter()
            .filter_map(|(pkh, alias)| {
                let key = &self.keys.get(alias)?;
                Some((alias.clone(), (*key, Some(pkh))))
            })
            .collect();
        self.keys.iter().for_each(|(alias, key)| {
            if !keys.contains_key(alias) {
                keys.insert(alias.clone(), (key, None));
            }
        });
        keys
    }

    /// Get all known addresses by their alias.
    pub fn get_addresses(&self) -> &BiHashMap<Alias, Address> {
        &self.addresses
    }

    /// Get all known payment addresses by their alias.
    pub fn get_payment_addrs(&self) -> &HashMap<Alias, PaymentAddress> {
        &self.payment_addrs
    }

    /// Get all known viewing keys by their alias.
    pub fn get_viewing_keys(&self) -> &HashMap<Alias, ExtendedViewingKey> {
        &self.view_keys
    }

    /// Get all known spending keys by their alias.
    pub fn get_spending_keys(
        &self,
    ) -> &HashMap<Alias, StoredKeypair<ExtendedSpendingKey>> {
        &self.spend_keys
    }

    fn generate_spending_key() -> ExtendedSpendingKey {
        use rand::rngs::OsRng;
        let mut spend_key = [0; 32];
        OsRng.fill_bytes(&mut spend_key);
        masp_primitives::zip32::ExtendedSpendingKey::master(spend_key.as_ref())
            .into()
    }

    /// Generate a new keypair and insert it into the store with the provided
    /// alias. If none provided, the alias will be the public key hash.
    /// If no encryption password is provided, the keypair will be stored raw
    /// without encryption.
    /// Optionally, use a given random seed and a BIP44 derivation path.
    /// Returns the alias of the key and a reference-counting pointer to the
    /// key.
    pub fn gen_key(
        &mut self,
        scheme: SchemeType,
        alias: Option<String>,
        seed_and_derivation_path: Option<(Seed, DerivationPath)>,
        encryption_password: Option<String>,
    ) -> (Alias, common::SecretKey) {
        let sk = if let Some((seed, derivation_path)) = seed_and_derivation_path
        {
            gen_sk_from_seed_and_derivation_path(
                scheme,
                seed.as_bytes(),
                derivation_path,
            )
        } else {
            gen_sk_rng(scheme)
        };
        let pkh: PublicKeyHash = PublicKeyHash::from(&sk.ref_to());
        let (keypair_to_store, raw_keypair) =
            StoredKeypair::new(sk, encryption_password);
        let address = Address::Implicit(ImplicitAddress(pkh.clone()));
        let alias: Alias = alias.unwrap_or_else(|| pkh.clone().into()).into();
        if self
            .insert_keypair(alias.clone(), keypair_to_store, pkh)
            .is_none()
        {
            eprintln!("Action cancelled, no changes persisted.");
            cli::safe_exit(1);
        }
        if self.insert_address(alias.clone(), address).is_none() {
            eprintln!("Action cancelled, no changes persisted.");
            cli::safe_exit(1);
        }
        (alias, raw_keypair)
    }

    /// Generate a spending key similarly to how it's done for keypairs
    pub fn gen_spending_key(
        &mut self,
        alias: String,
        password: Option<String>,
    ) -> (Alias, ExtendedSpendingKey) {
        let spendkey = Self::generate_spending_key();
        let viewkey = ExtendedFullViewingKey::from(&spendkey.into()).into();
        let (spendkey_to_store, _raw_spendkey) =
            StoredKeypair::new(spendkey, password);
        let alias = Alias::from(alias);
        if self
            .insert_spending_key(alias.clone(), spendkey_to_store, viewkey)
            .is_none()
        {
            eprintln!("Action cancelled, no changes persisted.");
            cli::safe_exit(1);
        }
        (alias, spendkey)
    }

    /// Generate keypair for signing protocol txs and for the DKG
    /// A protocol keypair may be optionally provided
    ///
    /// Note that this removes the validator data.
    pub fn gen_validator_keys(
        protocol_keypair: Option<common::SecretKey>,
        scheme: SchemeType,
    ) -> ValidatorKeys {
        let protocol_keypair =
            protocol_keypair.unwrap_or_else(|| gen_sk_rng(scheme));
        let dkg_keypair = ferveo_common::Keypair::<EllipticCurve>::new(
            &mut StdRng::from_entropy(),
        );
        ValidatorKeys {
            protocol_keypair,
            dkg_keypair: Some(dkg_keypair.into()),
        }
    }

    /// Add validator data to the store
    pub fn add_validator_data(
        &mut self,
        address: Address,
        keys: ValidatorKeys,
    ) {
        self.validator_data = Some(ValidatorData { address, keys });
    }

    /// Returns the validator data, if it exists
    pub fn get_validator_data(&self) -> Option<&ValidatorData> {
        self.validator_data.as_ref()
    }

    /// Returns the validator data, if it exists
    pub fn validator_data(self) -> Option<ValidatorData> {
        self.validator_data
    }

    /// Insert a new key with the given alias. If the alias is already used,
    /// will prompt for overwrite/reselection confirmation. If declined, then
    /// keypair is not inserted and nothing is returned, otherwise selected
    /// alias is returned.
    pub(super) fn insert_keypair(
        &mut self,
        alias: Alias,
        keypair: StoredKeypair<common::SecretKey>,
        pkh: PublicKeyHash,
    ) -> Option<Alias> {
        // abort if the key already exists
        if self.pkhs.contains_key(&pkh) {
            println!("The key already exists.");
            return None;
        }

        if alias.is_empty() {
            println!(
                "Empty alias given, defaulting to {}.",
                Into::<Alias>::into(pkh.to_string())
            );
        }
        // Addresses and keypairs can share aliases, so first remove any
        // addresses sharing the same namesake before checking if alias has been
        // used.
        let counterpart_address = self.addresses.remove_by_left(&alias);
        if self.contains_alias(&alias) {
            match show_overwrite_confirmation(&alias, "a key") {
                ConfirmationResponse::Replace => {}
                ConfirmationResponse::Reselect(new_alias) => {
                    // Restore the removed address in case the recursive prompt
                    // terminates with a cancellation
                    counterpart_address
                        .map(|x| self.addresses.insert(alias.clone(), x.1));
                    return self.insert_keypair(new_alias, keypair, pkh);
                }
                ConfirmationResponse::Skip => {
                    // Restore the removed address since this insertion action
                    // has now been cancelled
                    counterpart_address
                        .map(|x| self.addresses.insert(alias.clone(), x.1));
                    return None;
                }
            }
        }
        self.remove_alias(&alias);
        self.keys.insert(alias.clone(), keypair);
        self.pkhs.insert(pkh, alias.clone());
        // Since it is intended for the inserted keypair to share its namesake
        // with the pre-existing address
        counterpart_address.map(|x| self.addresses.insert(alias.clone(), x.1));
        Some(alias)
    }

    /// Insert spending keys similarly to how it's done for keypairs
    pub fn insert_spending_key(
        &mut self,
        alias: Alias,
        spendkey: StoredKeypair<ExtendedSpendingKey>,
        viewkey: ExtendedViewingKey,
    ) -> Option<Alias> {
        if alias.is_empty() {
            eprintln!("Empty alias given.");
            return None;
        }
        if self.contains_alias(&alias) {
            match show_overwrite_confirmation(&alias, "a spending key") {
                ConfirmationResponse::Replace => {}
                ConfirmationResponse::Reselect(new_alias) => {
                    return self
                        .insert_spending_key(new_alias, spendkey, viewkey);
                }
                ConfirmationResponse::Skip => return None,
            }
        }
        self.remove_alias(&alias);
        self.spend_keys.insert(alias.clone(), spendkey);
        // Simultaneously add the derived viewing key to ease balance viewing
        self.view_keys.insert(alias.clone(), viewkey);
        Some(alias)
    }

    /// Insert viewing keys similarly to how it's done for keypairs
    pub fn insert_viewing_key(
        &mut self,
        alias: Alias,
        viewkey: ExtendedViewingKey,
    ) -> Option<Alias> {
        if alias.is_empty() {
            eprintln!("Empty alias given.");
            return None;
        }
        if self.contains_alias(&alias) {
            match show_overwrite_confirmation(&alias, "a viewing key") {
                ConfirmationResponse::Replace => {}
                ConfirmationResponse::Reselect(new_alias) => {
                    return self.insert_viewing_key(new_alias, viewkey);
                }
                ConfirmationResponse::Skip => return None,
            }
        }
        self.remove_alias(&alias);
        self.view_keys.insert(alias.clone(), viewkey);
        Some(alias)
    }

    /// Check if any map of the wallet contains the given alias
    fn contains_alias(&self, alias: &Alias) -> bool {
        self.payment_addrs.contains_key(alias)
            || self.view_keys.contains_key(alias)
            || self.spend_keys.contains_key(alias)
            || self.keys.contains_key(alias)
            || self.addresses.contains_left(alias)
    }

    /// Completely remove the given alias from all maps in the wallet
    fn remove_alias(&mut self, alias: &Alias) {
        self.payment_addrs.remove(alias);
        self.view_keys.remove(alias);
        self.spend_keys.remove(alias);
        self.keys.remove(alias);
        self.addresses.remove_by_left(alias);
        self.pkhs.retain(|_key, val| val != alias);
    }

    /// Insert payment addresses similarly to how it's done for keypairs
    pub fn insert_payment_addr(
        &mut self,
        alias: Alias,
        payment_addr: PaymentAddress,
    ) -> Option<Alias> {
        if alias.is_empty() {
            eprintln!("Empty alias given.");
            return None;
        }
        if self.contains_alias(&alias) {
            match show_overwrite_confirmation(&alias, "a payment address") {
                ConfirmationResponse::Replace => {}
                ConfirmationResponse::Reselect(new_alias) => {
                    return self.insert_payment_addr(new_alias, payment_addr);
                }
                ConfirmationResponse::Skip => return None,
            }
        }
        self.remove_alias(&alias);
        self.payment_addrs.insert(alias.clone(), payment_addr);
        Some(alias)
    }

    /// Helper function to restore keypair given alias-keypair mapping and the
    /// pkhs-alias mapping.
    fn restore_keypair(
        &mut self,
        alias: Alias,
        key: Option<StoredKeypair<common::SecretKey>>,
        pkh: Option<PublicKeyHash>,
    ) {
        key.map(|x| self.keys.insert(alias.clone(), x));
        pkh.map(|x| self.pkhs.insert(x, alias.clone()));
    }

    /// Insert a new address with the given alias. If the alias is already used,
    /// will prompt for overwrite/reselection confirmation, which when declined,
    /// the address won't be added. Return the selected alias if the address has
    /// been added.
    pub fn insert_address(
        &mut self,
        alias: Alias,
        address: Address,
    ) -> Option<Alias> {
        if alias.is_empty() {
            println!("Empty alias given, defaulting to {}.", address.encode());
        }
        // Addresses and keypairs can share aliases, so first remove any keys
        // sharing the same namesake before checking if alias has been used.
        let counterpart_key = self.keys.remove(&alias);
        let mut counterpart_pkh = None;
        self.pkhs.retain(|k, v| {
            if v == &alias {
                counterpart_pkh = Some(k.clone());
                false
            } else {
                true
            }
        });
        if self.addresses.contains_left(&alias) {
            match show_overwrite_confirmation(&alias, "an address") {
                ConfirmationResponse::Replace => {}
                ConfirmationResponse::Reselect(new_alias) => {
                    // Restore the removed keypair in case the recursive prompt
                    // terminates with a cancellation
                    self.restore_keypair(
                        alias,
                        counterpart_key,
                        counterpart_pkh,
                    );
                    return self.insert_address(new_alias, address);
                }
                ConfirmationResponse::Skip => {
                    // Restore the removed keypair since this insertion action
                    // has now been cancelled
                    self.restore_keypair(
                        alias,
                        counterpart_key,
                        counterpart_pkh,
                    );
                    return None;
                }
            }
        }
        self.remove_alias(&alias);
        self.addresses.insert(alias.clone(), address);
        // Since it is intended for the inserted address to share its namesake
        // with the pre-existing keypair
        self.restore_keypair(alias.clone(), counterpart_key, counterpart_pkh);
        Some(alias)
    }

    /// Extend this store from pre-genesis validator wallet.
    pub fn extend_from_pre_genesis_validator(
        &mut self,
        validator_address: Address,
        validator_alias: Alias,
        other: pre_genesis::ValidatorWallet,
    ) {
        let account_key_alias = alias::validator_key(&validator_alias);
        let consensus_key_alias =
            alias::validator_consensus_key(&validator_alias);
        let tendermint_node_key_alias =
            alias::validator_tendermint_node_key(&validator_alias);

        let keys = [
            (account_key_alias.clone(), other.store.account_key),
            (consensus_key_alias.clone(), other.store.consensus_key),
            (
                tendermint_node_key_alias.clone(),
                other.store.tendermint_node_key,
            ),
        ];
        self.keys.extend(keys.into_iter());

        let account_pk = other.account_key.ref_to();
        let consensus_pk = other.consensus_key.ref_to();
        let tendermint_node_pk = other.tendermint_node_key.ref_to();
        let addresses = [
            (account_key_alias.clone(), (&account_pk).into()),
            (consensus_key_alias.clone(), (&consensus_pk).into()),
            (
                tendermint_node_key_alias.clone(),
                (&tendermint_node_pk).into(),
            ),
        ];
        self.addresses.extend(addresses.into_iter());

        let pkhs = [
            ((&account_pk).into(), account_key_alias),
            ((&consensus_pk).into(), consensus_key_alias),
            ((&tendermint_node_pk).into(), tendermint_node_key_alias),
        ];
        self.pkhs.extend(pkhs.into_iter());

        self.validator_data = Some(ValidatorData {
            address: validator_address,
            keys: other.store.validator_keys,
        });
    }

    pub fn get_addresses_with_vp_type(
        &self,
        vp_type: AddressVpType,
    ) -> HashSet<Address> {
        // defaults to an empty set
        self.address_vp_types
            .get(&vp_type)
            .cloned()
            .unwrap_or_default()
    }

    pub fn add_vp_type_to_address(
        &mut self,
        vp_type: AddressVpType,
        address: Address,
    ) {
        // defaults to an empty set
        self.address_vp_types
            .entry(vp_type)
            .or_default()
            .insert(address);
    }

    fn decode(data: Vec<u8>) -> Result<Self, toml::de::Error> {
        toml::from_slice(&data)
    }

    fn encode(&self) -> Vec<u8> {
        toml::to_vec(self).expect("Serializing of store shouldn't fail")
    }
}

enum ConfirmationResponse {
    Replace,
    Reselect(Alias),
    Skip,
}

/// The given alias has been selected but conflicts with another alias in
/// the store. Offer the user to either replace existing mapping, alter the
/// chosen alias to a name of their chosing, or cancel the aliasing.

fn show_overwrite_confirmation(
    alias: &Alias,
    alias_for: &str,
) -> ConfirmationResponse {
    print!(
        "You're trying to create an alias \"{}\" that already exists for {} \
         in your store.\nWould you like to replace it? \
         s(k)ip/re(p)lace/re(s)elect: ",
        alias, alias_for
    );
    io::stdout().flush().unwrap();

    let mut buffer = String::new();
    // Get the user to select between 3 choices
    match io::stdin().read_line(&mut buffer) {
        Ok(size) if size > 0 => {
            // Isolate the single character representing the choice
            let byte = buffer.chars().next().unwrap();
            buffer.clear();
            match byte {
                'p' | 'P' => return ConfirmationResponse::Replace,
                's' | 'S' => {
                    // In the case of reselection, elicit new alias
                    print!("Please enter a different alias: ");
                    io::stdout().flush().unwrap();
                    if io::stdin().read_line(&mut buffer).is_ok() {
                        return ConfirmationResponse::Reselect(
                            buffer.trim().into(),
                        );
                    }
                }
                'k' | 'K' => return ConfirmationResponse::Skip,
                // Input is senseless fall through to repeat prompt
                _ => {}
            };
        }
        _ => {}
    }
    // Input is senseless fall through to repeat prompt
    println!("Invalid option, try again.");
    show_overwrite_confirmation(alias, alias_for)
}

/// Wallet file name
const FILE_NAME: &str = "wallet.toml";

/// Get the path to the wallet store.
pub fn wallet_file(store_dir: impl AsRef<Path>) -> PathBuf {
    store_dir.as_ref().join(FILE_NAME)
}

impl Display for AddressVpType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddressVpType::Token => write!(f, "token"),
        }
    }
}

impl FromStr for AddressVpType {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "token" => Ok(Self::Token),
            _ => Err("unexpected address VP type"),
        }
    }
}

impl Serialize for AddressVpType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AddressVpType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let raw: String = Deserialize::deserialize(deserializer)?;
        Self::from_str(&raw).map_err(D::Error::custom)
    }
}

/// Generate a new secret key.
pub fn gen_sk_rng(scheme: SchemeType) -> common::SecretKey {
    use rand::rngs::OsRng;
    let mut csprng = OsRng {};
    match scheme {
        SchemeType::Ed25519 => ed25519::SigScheme::generate(&mut csprng)
            .try_to_sk()
            .unwrap(),
        SchemeType::Secp256k1 => secp256k1::SigScheme::generate(&mut csprng)
            .try_to_sk()
            .unwrap(),
        SchemeType::Common => common::SigScheme::generate(&mut csprng)
            .try_to_sk()
            .unwrap(),
    }
}

/// Generate a new secret key from the seed.
pub fn gen_sk_from_seed_and_derivation_path(
    scheme: SchemeType,
    seed: &[u8],
    derivation_path: DerivationPath,
) -> common::SecretKey {
    match scheme {
        SchemeType::Ed25519 => {
            let indexes = derivation_path
                .path()
                .iter()
                .map(|idx| idx.to_bits())
                .collect_vec();
            // SLIP10 Ed25519 key derivation function promotes all indexes to
            // hardened indexes.
            let sk = slip10_ed25519::derive_ed25519_private_key(seed, &indexes);
            ed25519::SigScheme::from_bytes(sk).try_to_sk().unwrap()
        }
        SchemeType::Secp256k1 => {
            let xpriv = tiny_hderive::bip32::ExtendedPrivKey::derive(
                seed,
                derivation_path,
            )
            .expect("Secret key derivation should not fail.");
            secp256k1::SigScheme::from_bytes(xpriv.secret())
                .try_to_sk()
                .unwrap()
        }
        SchemeType::Common => {
            panic!(
                "Cannot generate common signing scheme. Must convert from \
                 alternative scheme."
            )
        }
    }
}

#[cfg(all(test, feature = "dev"))]
mod test_wallet {
    use base58::{self, FromBase58};
    use bip39::{Language, Mnemonic};
    use hex;

    use super::*;

    #[test]
    fn test_toml_roundtrip_ed25519() {
        let mut store = Store::new();
        let validator_keys =
            Store::gen_validator_keys(None, SchemeType::Ed25519);
        store.add_validator_data(
            Address::decode("atest1v4ehgw36x3prswzxggunzv6pxqmnvdj9xvcyzvpsggeyvs3cg9qnywf589qnwvfsg5erg3fkl09rg5").unwrap(),
            validator_keys
        );
        let data = store.encode();
        let _ = Store::decode(data).expect("Test failed");
    }

    #[test]
    fn test_toml_roundtrip_secp256k1() {
        let mut store = Store::new();
        let validator_keys =
            Store::gen_validator_keys(None, SchemeType::Secp256k1);
        store.add_validator_data(
            Address::decode("atest1v4ehgw36x3prswzxggunzv6pxqmnvdj9xvcyzvpsggeyvs3cg9qnywf589qnwvfsg5erg3fkl09rg5").unwrap(),
            validator_keys
        );
        let data = store.encode();
        let _ = Store::decode(data).expect("Test failed");
    }

    #[test]
    fn gen_sk_from_mnemonic_code_secp256k1() {
        const SCHEME: SchemeType = SchemeType::Secp256k1;
        const MNEMONIC_CODE: &str = "cruise ball fame lucky fabric govern \
                                     length fruit permit tonight fame pear \
                                     horse park key chimney furnace lobster \
                                     foot example shoot dry fuel lawn";
        const SEED_EXPECTED: &str = "8601e9f639f995f856c8ecfa3cd298d292253d071d6fa75d50dd31f6ba9df743ec279354a73fe78e6011738027fb994e5d904c476c4679c7c35e82586e14297f";
        const PASSPHRASE: &str = "test";
        const DERIVATION_PATH: &str = "m/44'/60'/0'/0/0";
        const SK_EXPECTED: &str =
            "9e426cc5f63cdbd5f362adb918a07c2b16c593b2bc1b244f33b9e2c3ac6b265a";

        let mnemonic = Mnemonic::from_phrase(MNEMONIC_CODE, Language::English)
            .expect("Mnemonic construction cannot fail.");
        let seed = Seed::new(&mnemonic, PASSPHRASE);
        assert_eq!(format!("{:x}", seed), SEED_EXPECTED);

        let derivation_path =
            DerivationPath::from_path_str(SCHEME, DERIVATION_PATH)
                .expect("Derivation path construction cannot fail");

        let sk = gen_sk_from_seed_and_derivation_path(
            SCHEME,
            seed.as_bytes(),
            derivation_path,
        );

        assert_eq!(&sk.to_string()[2..], SK_EXPECTED);
    }

    #[test]
    fn gen_sk_from_mnemonic_code_ed25519() {
        const SCHEME: SchemeType = SchemeType::Ed25519;
        const MNEMONIC_CODE: &str = "cruise ball fame lucky fabric govern \
                                     length fruit permit tonight fame pear \
                                     horse park key chimney furnace lobster \
                                     foot example shoot dry fuel lawn";
        const SEED_EXPECTED: &str = "8601e9f639f995f856c8ecfa3cd298d292253d071d6fa75d50dd31f6ba9df743ec279354a73fe78e6011738027fb994e5d904c476c4679c7c35e82586e14297f";
        const PASSPHRASE: &str = "test";
        const DERIVATION_PATH: &str = "m/44'/877'/0'/0/0";
        const DERIVATION_PATH_HARDENED: &str = "m/44'/877'/0'/0'/0'";

        let mnemonic = Mnemonic::from_phrase(MNEMONIC_CODE, Language::English)
            .expect("Mnemonic construction cannot fail.");
        let seed = Seed::new(&mnemonic, PASSPHRASE);
        assert_eq!(format!("{:x}", seed), SEED_EXPECTED);

        let derivation_path =
            DerivationPath::from_path_str(SCHEME, DERIVATION_PATH)
                .expect("Derivation path construction cannot fail");

        let derivation_path_hardened =
            DerivationPath::from_path_str(SCHEME, DERIVATION_PATH_HARDENED)
                .expect("Derivation path construction cannot fail");

        let sk = gen_sk_from_seed_and_derivation_path(
            SCHEME,
            seed.as_bytes(),
            derivation_path,
        );

        let sk_hard = gen_sk_from_seed_and_derivation_path(
            SCHEME,
            seed.as_bytes(),
            derivation_path_hardened,
        );

        // check that indexes are promoted to hardened
        assert_eq!(&sk.to_string(), &sk_hard.to_string());
    }

    fn do_test_gen_sk_from_seed_and_derivation_path(
        scheme: SchemeType,
        seed: &str,
        derivation_path: &str,
        priv_key: &str,
    ) {
        let sk = gen_sk_from_seed_and_derivation_path(
            scheme,
            hex::decode(seed)
                .expect("Seed parsing cannot fail.")
                .as_slice(),
            DerivationPath::from_path_str(scheme, derivation_path)
                .expect("Derivation path construction cannot fail"),
        );
        let sk_expected = if priv_key.starts_with("xprv") {
            // this is an extended private key encoded in base58
            let xprv =
                priv_key.from_base58().expect("XPRV parsing cannot fail.");
            hex::encode(&xprv[46..78])
        } else {
            priv_key.to_string()
        };
        assert_eq!(&sk.to_string()[2..], sk_expected);
    }

    #[test]
    fn gen_sk_from_seed_secp256k1() {
        const SCHEME: SchemeType = SchemeType::Secp256k1;
        // https://github.com/bitcoin/bips/blob/master/bip-0032.mediawiki
        {
            // Test vector 1
            const SEED: &str = "000102030405060708090a0b0c0d0e0f";
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m", "xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi");
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m/0'", "xprv9uHRZZhk6KAJC1avXpDAp4MDc3sQKNxDiPvvkX8Br5ngLNv1TxvUxt4cV1rGL5hj6KCesnDYUhd7oWgT11eZG7XnxHrnYeSvkzY7d2bhkJ7");
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m/0'/1", "xprv9wTYmMFdV23N2TdNG573QoEsfRrWKQgWeibmLntzniatZvR9BmLnvSxqu53Kw1UmYPxLgboyZQaXwTCg8MSY3H2EU4pWcQDnRnrVA1xe8fs");
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m/0'/1/2'", "xprv9z4pot5VBttmtdRTWfWQmoH1taj2axGVzFqSb8C9xaxKymcFzXBDptWmT7FwuEzG3ryjH4ktypQSAewRiNMjANTtpgP4mLTj34bhnZX7UiM");
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m/0'/1/2'/2", "xprvA2JDeKCSNNZky6uBCviVfJSKyQ1mDYahRjijr5idH2WwLsEd4Hsb2Tyh8RfQMuPh7f7RtyzTtdrbdqqsunu5Mm3wDvUAKRHSC34sJ7in334");
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m/0'/1/2'/2/1000000000", "xprvA41z7zogVVwxVSgdKUHDy1SKmdb533PjDz7J6N6mV6uS3ze1ai8FHa8kmHScGpWmj4WggLyQjgPie1rFSruoUihUZREPSL39UNdE3BBDu76");
        }
    }

    #[test]
    fn gen_sk_from_seed_ed25519() {
        const SCHEME: SchemeType = SchemeType::Ed25519;
        // https://github.com/satoshilabs/slips/blob/master/slip-0010.md
        {
            // Test vector 1 for ed15519
            const SEED: &str = "000102030405060708090a0b0c0d0e0f";
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m", "2b4be7f19ee27bbf30c667b642d5f4aa69fd169872f8fc3059c08ebae2eb19e7");
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m/0'", "68e0fe46dfb67e368c75379acec591dad19df3cde26e63b93a8e704f1dade7a3");
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m/0'/1'", "b1d0bad404bf35da785a64ca1ac54b2617211d2777696fbffaf208f746ae84f2");
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m/0'/1'/2'", "92a5b23c0b8a99e37d07df3fb9966917f5d06e02ddbd909c7e184371463e9fc9");
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m/0'/1'/2'/2'", "30d1dc7e5fc04c31219ab25a27ae00b50f6fd66622f6e9c913253d6511d1e662");
            do_test_gen_sk_from_seed_and_derivation_path(SCHEME, SEED, "m/0'/1'/2'/2'/1000000000'", "8f94d394a8e8fd6b1bc2f3f49f5c47e385281d5c17e65324b0f62483e37e8793");
        }
    }
}
