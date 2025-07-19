use std::path::PathBuf;
use std::sync::RwLock;

use lightning::{
    types::payment::{PaymentHash, PaymentPreimage},
    util::persist::KVStore,
};
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const PAYMENTS_PRIMARY_NAMESPACE: &str = "payments";
const PAYMENTS_SECONDARY_NAMESPACE: &str = "";

fn serialize_payment_hash<S>(hash: &PaymentHash, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_bytes(&hash.0)
}

fn deserialize_payment_hash<'de, D>(deserializer: D) -> Result<PaymentHash, D::Error>
where
    D: Deserializer<'de>,
{
    let bytes = <Vec<u8>>::deserialize(deserializer)?;
    if bytes.len() != 32 {
        return Err(serde::de::Error::custom("PaymentHash must be 32 bytes"));
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes);
    Ok(PaymentHash(hash))
}

// Custom serialization functions for PaymentPreimage
fn serialize_payment_preimage<S>(
    preimage: &Option<PaymentPreimage>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match preimage {
        Some(p) => serializer.serialize_some(&p.0),
        None => serializer.serialize_none(),
    }
}

fn deserialize_payment_preimage<'de, D>(
    deserializer: D,
) -> Result<Option<PaymentPreimage>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt_bytes: Option<Vec<u8>> = Option::deserialize(deserializer)?;
    match opt_bytes {
        Some(bytes) => {
            if bytes.len() != 32 {
                return Err(serde::de::Error::custom("PaymentPreimage must be 32 bytes"));
            }
            let mut preimage = [0u8; 32];
            preimage.copy_from_slice(&bytes);
            Ok(Some(PaymentPreimage(preimage)))
        }
        None => Ok(None),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaymentDirection {
    Inbound,
    Outbound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaymentStatus {
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payment {
    #[serde(
        serialize_with = "serialize_payment_hash",
        deserialize_with = "deserialize_payment_hash"
    )]
    pub payment_hash: PaymentHash,
    #[serde(
        serialize_with = "serialize_payment_preimage",
        deserialize_with = "deserialize_payment_preimage"
    )]
    pub payment_preimage: Option<PaymentPreimage>,
    pub direction: PaymentDirection,
    pub amount_msat: u64,
    pub fee_paid_msat: Option<u64>,
    // TODO: re-add these fields
    // pub payment_id: String,
    pub status: PaymentStatus,
    // pub created_at: u64,
    // pub updated_at: u64,
}

// Another store for forwarded payments?

pub struct PaymentStore {
    store: RwLock<FilesystemStore>,
}

impl PaymentStore {
    pub fn new(data_dir: PathBuf) -> Self {
        let store = RwLock::new(FilesystemStore::new(data_dir));
        Self { store }
    }

    // NOTE: could do something better than json. Just doing because easy... and this is not
    // intended for anything important.

    pub fn add_payment(&self, payment: Payment) -> Result<(), lightning::io::Error> {
        let key = hex::encode(payment.payment_hash.0);
        let serialized = serde_json::to_vec(&payment)
            .map_err(|e| lightning::io::Error::new(lightning::io::ErrorKind::InvalidData, e))?;

        self.store.write().unwrap().write(
            PAYMENTS_PRIMARY_NAMESPACE,
            PAYMENTS_SECONDARY_NAMESPACE,
            &key,
            &serialized,
        )
    }

    pub fn update_payment(&self, payment: Payment) -> Result<(), lightning::io::Error> {
        self.add_payment(payment)
    }

    pub fn get_payment(&self, payment_hash: &PaymentHash) -> Result<Payment, lightning::io::Error> {
        let key = hex::encode(payment_hash.0);

        match self.store.read().unwrap().read(
            PAYMENTS_PRIMARY_NAMESPACE,
            PAYMENTS_SECONDARY_NAMESPACE,
            &key,
        ) {
            Ok(data) => {
                let payment: Payment = serde_json::from_slice(&data).map_err(|e| {
                    lightning::io::Error::new(lightning::io::ErrorKind::InvalidData, e)
                })?;
                Ok(payment)
            }
            Err(e) => Err(e),
        }
    }

    // Don't like that this returns an error but meh.
    pub fn list_payments(&self) -> Result<Vec<Payment>, lightning::io::Error> {
        let keys = self
            .store
            .read()
            .unwrap()
            .list(PAYMENTS_PRIMARY_NAMESPACE, PAYMENTS_SECONDARY_NAMESPACE)?;
        let mut payments = Vec::new();

        for key in keys {
            match self.store.read().unwrap().read(
                PAYMENTS_PRIMARY_NAMESPACE,
                PAYMENTS_SECONDARY_NAMESPACE,
                &key,
            ) {
                Ok(data) => match serde_json::from_slice::<Payment>(&data) {
                    Ok(payment) => payments.push(payment),
                    Err(_) => continue,
                },
                Err(_) => continue,
            }
        }

        Ok(payments)
    }

    pub fn remove_payment(&self, payment_hash: &PaymentHash) -> Result<(), lightning::io::Error> {
        let key = hex::encode(payment_hash.0);
        self.store.write().unwrap().remove(
            PAYMENTS_PRIMARY_NAMESPACE,
            PAYMENTS_SECONDARY_NAMESPACE,
            &key,
            true,
        )
    }

    pub fn list_payments_by_status(
        &self,
        status: PaymentStatus,
    ) -> Result<Vec<Payment>, lightning::io::Error> {
        let all_payments = self.list_payments()?;
        Ok(all_payments.into_iter()
            .filter(|p| matches!(p.status, ref s if std::mem::discriminant(s) == std::mem::discriminant(&status)))
            .collect())
    }
}
