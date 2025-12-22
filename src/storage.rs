use std::path::PathBuf;
use std::sync::RwLock;

use bitcoin::{io::ErrorKind, secp256k1::PublicKey};
use lightning::{
    _init_and_read_len_prefixed_tlv_fields, impl_writeable_tlv_based_enum,
    io::{self, Cursor},
    ln::{
        channelmanager::PaymentId,
        msgs::{DecodeError, SocketAddress},
    },
    types::payment::{PaymentHash, PaymentPreimage},
    util::{
        persist::KVStore,
        ser::{Readable, Writeable, Writer},
    },
    write_tlv_fields,
};
use lightning_persister::fs_store::FilesystemStore;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_bytes::Bytes;

const PAYMENTS_PRIMARY_NAMESPACE: &str = "payments";
const PAYMENTS_SECONDARY_NAMESPACE: &str = "";

const PEERS_PRIMARY_NAMESPACE: &str = "peers";
const PEERS_SECONDARY_NAMESPACE: &str = "";

#[derive(Debug, Serialize)]
pub enum PaymentDirection {
    Inbound,
    Outbound,
}

impl_writeable_tlv_based_enum!(PaymentDirection,
    (0, Inbound) => {},
    (2, Outbound) => {},
);

#[derive(Debug, PartialEq, Eq, Serialize)]
pub enum PaymentStatus {
    Pending,
    Succeeded,
    Failed,
}

impl_writeable_tlv_based_enum!(PaymentStatus,
    (0, Pending) => {},
    (2, Succeeded) => {},
    (4, Failed) => {},
);

pub struct Payment {
    pub payment_id: PaymentId,
    pub payment_hash: PaymentHash,
    pub payment_preimage: Option<PaymentPreimage>,
    pub direction: PaymentDirection,
    pub amount_msat: Option<u64>,
    pub fee_paid_msat: Option<u64>,
    pub status: PaymentStatus,
    // TODO: re-add these fields
    // pub created_at: u64,
    // pub updated_at: u64,
}

// impl Serialize here as well for json output

impl Writeable for Payment {
    fn write<W: Writer>(&self, writer: &mut W) -> Result<(), io::Error> {
        write_tlv_fields!(writer, {
            (0, self.payment_id, required),
            (2, self.payment_hash, required),
            (4, self.payment_preimage, required),
            (6, self.direction, required),
            (8, self.amount_msat, required),
            (10, self.fee_paid_msat, required),
            (12, self.status, required),
        });
        Ok(())
    }
}

impl Readable for Payment {
    fn read<R: io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
        _init_and_read_len_prefixed_tlv_fields!(reader, {
            (0, payment_id, required),
            (2, payment_hash, required),
            (4, payment_preimage, required),
            (6, direction, required),
            (8, amount_msat, required),
            (10, fee_paid_msat, required),
            (12, status, required),
        });

        Ok(Payment {
            payment_id: payment_id.0.ok_or(DecodeError::InvalidValue)?,
            payment_hash: payment_hash.0.ok_or(DecodeError::InvalidValue)?,
            payment_preimage: payment_preimage.0.ok_or(DecodeError::InvalidValue)?,
            direction: direction.0.ok_or(DecodeError::InvalidValue)?,
            amount_msat: amount_msat.0.ok_or(DecodeError::InvalidValue)?,
            fee_paid_msat: fee_paid_msat.0.ok_or(DecodeError::InvalidValue)?,
            status: status.0.ok_or(DecodeError::InvalidValue)?,
        })
    }
}

impl Serialize for Payment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Payment", 6)?;

        state.serialize_field("payment_id", &Bytes::new(&self.payment_id.0))?;
        state.serialize_field("payment_hash", &Bytes::new(&self.payment_hash.0))?;
        state.serialize_field(
            "payment_preimage",
            &self.payment_preimage.as_ref().map(|p| Bytes::new(&p.0)),
        )?;

        state.serialize_field("direction", &self.direction)?;
        state.serialize_field("amount_msat", &self.amount_msat)?;
        state.serialize_field("fee_paid_msat", &self.fee_paid_msat)?;
        state.serialize_field("status", &self.status)?;

        state.end()
    }
}

#[derive(Debug)]
pub struct PeerInfo {
    pub node_id: PublicKey,
    pub address: SocketAddress,
}

impl Writeable for PeerInfo {
    fn write<W: Writer>(&self, writer: &mut W) -> Result<(), io::Error> {
        write_tlv_fields!(writer, {
            (0, self.node_id, required),
            (2, self.address, required),
        });
        Ok(())
    }
}

impl Readable for PeerInfo {
    fn read<R: io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
        _init_and_read_len_prefixed_tlv_fields!(reader, {
            (0, node_id, required),
            (2, address, required),
        });

        Ok(PeerInfo {
            node_id: node_id.0.ok_or(DecodeError::InvalidValue)?,
            address: address.0.ok_or(DecodeError::InvalidValue)?,
        })
    }
}

// Another store for forwarded payments?

pub struct NodeStore {
    store: RwLock<FilesystemStore>,
}

impl NodeStore {
    pub fn new(data_dir: PathBuf) -> Self {
        let store = RwLock::new(FilesystemStore::new(data_dir));
        Self { store }
    }

    pub fn add_payment(&self, payment: Payment) -> Result<(), io::Error> {
        let key = hex::encode(payment.payment_id.0);
        let serialized = payment.encode();

        self.store.write().unwrap().write(
            PAYMENTS_PRIMARY_NAMESPACE,
            PAYMENTS_SECONDARY_NAMESPACE,
            &key,
            &serialized,
        )
    }

    pub fn update_payment(&self, payment: Payment) -> Result<(), io::Error> {
        self.add_payment(payment)
    }

    pub fn get_payment(&self, payment_id: &PaymentId) -> Result<Payment, io::Error> {
        let key = hex::encode(payment_id.0);

        match self.store.read().unwrap().read(
            PAYMENTS_PRIMARY_NAMESPACE,
            PAYMENTS_SECONDARY_NAMESPACE,
            &key,
        ) {
            Ok(data) => {
                let mut reader = Cursor::new(data);
                let payment = Payment::read(&mut reader).map_err(|_| {
                    io::Error::new(ErrorKind::InvalidData, "Could not deserialize payment")
                })?;
                Ok(payment)
            }
            Err(e) => Err(e),
        }
    }

    // Don't like that this returns an error but meh.
    pub fn list_payments(&self) -> Result<Vec<Payment>, io::Error> {
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
                Ok(data) => {
                    let mut reader = Cursor::new(data);
                    let payment = Payment::read(&mut reader).map_err(|_| {
                        io::Error::new(ErrorKind::InvalidData, "Could not deserialize payment")
                    })?;
                    payments.push(payment);
                }
                Err(_) => continue,
            }
        }

        Ok(payments)
    }

    pub fn remove_payment(&self, payment_id: &PaymentId) -> Result<(), io::Error> {
        let key = hex::encode(payment_id.0);
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
    ) -> Result<Vec<Payment>, io::Error> {
        let all_payments = self.list_payments()?;
        Ok(all_payments.into_iter()
            .filter(|p| matches!(p.status, ref s if std::mem::discriminant(s) == std::mem::discriminant(&status)))
            .collect())
    }

    pub fn add_peer(&self, peer: &PeerInfo) -> Result<(), io::Error> {
        let key = hex::encode(peer.node_id.serialize());
        let serialized = peer.encode();

        self.store.write().unwrap().write(
            PEERS_PRIMARY_NAMESPACE,
            PEERS_SECONDARY_NAMESPACE,
            &key,
            &serialized,
        )
    }

    pub fn remove_peer(&self, pubkey: &PublicKey) -> Result<(), io::Error> {
        let key = hex::encode(pubkey.serialize());
        self.store.write().unwrap().remove(
            PEERS_PRIMARY_NAMESPACE,
            PEERS_SECONDARY_NAMESPACE,
            &key,
            true,
        )
    }

    pub fn list_peers(&self) -> Result<Vec<PeerInfo>, lightning::io::Error> {
        let keys = self
            .store
            .read()
            .unwrap()
            .list(PEERS_PRIMARY_NAMESPACE, PEERS_SECONDARY_NAMESPACE)?;
        let mut peers = Vec::new();

        for key in keys {
            match self.store.read().unwrap().read(
                PEERS_PRIMARY_NAMESPACE,
                PEERS_SECONDARY_NAMESPACE,
                &key,
            ) {
                Ok(data) => {
                    let mut reader = Cursor::new(data);
                    let peer = PeerInfo::read(&mut reader).map_err(|_| {
                        io::Error::new(ErrorKind::InvalidData, "Could not deserialize peer info")
                    })?;
                    peers.push(peer);
                }
                Err(e) => return Err(e),
            }
        }

        Ok(peers)
    }
}
