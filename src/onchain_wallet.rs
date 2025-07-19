use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bdk_wallet::bitcoin::Network;
use bdk_wallet::descriptor::template::Bip84;
use bdk_wallet::{
    ChangeSet, KeychainKind, PersistedWallet, Wallet as BdkWallet, file_store::Store,
};
use bip39::Mnemonic;
use bitcoin::bip32::ChildNumber;
use bitcoin::key::Secp256k1;
use bitcoin::key::rand::rngs::OsRng;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::RecoverableSignature;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::secp256k1::schnorr::Signature;
use bitcoin::secp256k1::{PublicKey, Scalar, ecdsa};
use bitcoin::{Address, NetworkKind, ScriptBuf};
use lightning::bolt11_invoice::RawBolt11Invoice;
use lightning::ln::inbound_payment::ExpandedKey;
use lightning::ln::msgs::{DecodeError, UnsignedGossipMessage};
use lightning::ln::script::ShutdownScript;
use lightning::offers::invoice::UnsignedBolt12Invoice;
use lightning::sign::{
    EntropySource, InMemorySigner, KeysManager, NodeSigner, Recipient, SignerProvider,
};

const SEED_FILENAME: &str = "seed";
const WALLET_FILENAME: &str = "wallet";

pub struct WalletManager {
    ldk_keys_manager: KeysManager,
    pub(crate) bdk_wallet: Arc<Mutex<PersistedWallet<Store<ChangeSet>>>>,
}

impl WalletManager {
    pub fn new(path: PathBuf, network: Network) -> Result<Self, Box<dyn Error>> {
        let wallet_path = path.join(WALLET_FILENAME);
        let seed_path = path.join(SEED_FILENAME);

        let mnemonic = if seed_path.exists() {
            mnemonic_from_file(&seed_path)?
        } else {
            let mut entropy = [0u8; 32];
            OsRng.fill_bytes(&mut entropy);
            Mnemonic::from_entropy(&entropy)?
        };

        let seed = mnemonic.to_seed_normalized("");

        let (mut store, _) = Store::load_or_create("lightning-node".as_bytes(), &wallet_path)?;

        let xpriv = bitcoin::bip32::Xpriv::new_master(NetworkKind::Test, &seed)?;
        // BIP-84 P2WPKH
        let descriptor = Bip84(xpriv, KeychainKind::External);
        let change_descriptor = Bip84(xpriv, KeychainKind::Internal);

        let wallet_opt = BdkWallet::load()
            .descriptor(KeychainKind::External, Some(descriptor.clone()))
            .descriptor(KeychainKind::Internal, Some(change_descriptor.clone()))
            .extract_keys()
            .check_network(network)
            .load_wallet(&mut store)?;

        let wallet = match wallet_opt {
            Some(wallet) => wallet,
            None => {
                fs::write(seed_path, mnemonic.to_string())?;
                BdkWallet::create(descriptor, change_descriptor)
                    .network(network)
                    .create_wallet(&mut store)?
            }
        };

        let wallet = Arc::new(Mutex::new(wallet));
        let secp = Secp256k1::new();

        // path for ldk keys
        let ldk_xpriv = xpriv.derive_priv(&secp, &ChildNumber::Hardened { index: 535 })?;
        let ldk_seed = ldk_xpriv.private_key.secret_bytes();
        let cur = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        let ldk_keys_manager = KeysManager::new(&ldk_seed, cur.as_secs(), cur.subsec_nanos());

        Ok(Self {
            ldk_keys_manager,
            bdk_wallet: wallet,
        })
    }

    pub fn build_transaction(&self, amount: u64, dest: ScriptBuf) -> Result<(), ()> {
        unimplemented!()
    }

    pub fn next_address(&self) -> Address {
        self.bdk_wallet
            .lock()
            .unwrap()
            .next_unused_address(KeychainKind::External)
            .address
    }
}

fn mnemonic_from_file(seed_path: &PathBuf) -> Result<Mnemonic, Box<dyn std::error::Error>> {
    let mnemonic = fs::read_to_string(seed_path)?;
    let mnemonic = Mnemonic::parse(&mnemonic)?;
    Ok(mnemonic)
}

impl EntropySource for WalletManager {
    fn get_secure_random_bytes(&self) -> [u8; 32] {
        self.ldk_keys_manager.get_secure_random_bytes()
    }
}

impl NodeSigner for WalletManager {
    fn get_inbound_payment_key(&self) -> ExpandedKey {
        self.ldk_keys_manager.get_inbound_payment_key()
    }

    fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
        self.ldk_keys_manager.get_node_id(recipient)
    }

    fn ecdh(
        &self,
        recipient: Recipient,
        other_key: &PublicKey,
        tweak: Option<&Scalar>,
    ) -> Result<SharedSecret, ()> {
        self.ldk_keys_manager.ecdh(recipient, other_key, tweak)
    }

    fn sign_invoice(
        &self,
        invoice: &RawBolt11Invoice,
        recipient: Recipient,
    ) -> Result<RecoverableSignature, ()> {
        self.ldk_keys_manager.sign_invoice(invoice, recipient)
    }

    fn sign_bolt12_invoice(&self, invoice: &UnsignedBolt12Invoice) -> Result<Signature, ()> {
        self.ldk_keys_manager.sign_bolt12_invoice(invoice)
    }

    fn sign_gossip_message(&self, msg: UnsignedGossipMessage<'_>) -> Result<ecdsa::Signature, ()> {
        self.ldk_keys_manager.sign_gossip_message(msg)
    }
}

impl SignerProvider for WalletManager {
    type EcdsaSigner = InMemorySigner;

    fn generate_channel_keys_id(
        &self,
        inbound: bool,
        channel_value_satoshis: u64,
        user_channel_id: u128,
    ) -> [u8; 32] {
        self.ldk_keys_manager.generate_channel_keys_id(
            inbound,
            channel_value_satoshis,
            user_channel_id,
        )
    }

    fn derive_channel_signer(
        &self,
        channel_value_satoshis: u64,
        channel_keys_id: [u8; 32],
    ) -> Self::EcdsaSigner {
        self.ldk_keys_manager
            .derive_channel_signer(channel_value_satoshis, channel_keys_id)
    }

    fn read_chan_signer(&self, reader: &[u8]) -> Result<Self::EcdsaSigner, DecodeError> {
        self.ldk_keys_manager.read_chan_signer(reader)
    }

    fn get_destination_script(&self, _channel_keys_id: [u8; 32]) -> Result<ScriptBuf, ()> {
        let address = self
            .bdk_wallet
            .lock()
            .unwrap()
            .next_unused_address(KeychainKind::External);
        Ok(address.script_pubkey())
    }

    fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
        let address = self
            .bdk_wallet
            .lock()
            .unwrap()
            .next_unused_address(KeychainKind::External);
        Ok(address.script_pubkey().try_into().unwrap())
    }
}
