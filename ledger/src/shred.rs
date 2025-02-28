//! The `shred` module defines data structures and methods to pull MTU sized data frames from the
//! network. There are two types of shreds: data and coding. Data shreds contain entry information
//! while coding shreds provide redundancy to protect against dropped network packets (erasures).
//!
//! +---------------------------------------------------------------------------------------------+
//! | Data Shred                                                                                  |
//! +---------------------------------------------------------------------------------------------+
//! | common       | data       | payload                                                         |
//! | header       | header     |                                                                 |
//! |+---+---+---  |+---+---+---|+----------------------------------------------------------+----+|
//! || s | s | .   || p | f | s || data (ie ledger entries)                                 | r  ||
//! || i | h | .   || a | l | i ||                                                          | e  ||
//! || g | r | .   || r | a | z || See notes immediately after shred diagrams for an        | s  ||
//! || n | e |     || e | g | e || explanation of the "restricted" section in this payload  | t  ||
//! || a | d |     || n | s |   ||                                                          | r  ||
//! || t |   |     || t |   |   ||                                                          | i  ||
//! || u | t |     ||   |   |   ||                                                          | c  ||
//! || r | y |     || o |   |   ||                                                          | t  ||
//! || e | p |     || f |   |   ||                                                          | e  ||
//! ||   | e |     || f |   |   ||                                                          | d  ||
//! |+---+---+---  |+---+---+---+|----------------------------------------------------------+----+|
//! +---------------------------------------------------------------------------------------------+
//!
//! +---------------------------------------------------------------------------------------------+
//! | Coding Shred                                                                                |
//! +---------------------------------------------------------------------------------------------+
//! | common       | coding     | payload                                                         |
//! | header       | header     |                                                                 |
//! |+---+---+---  |+---+---+---+----------------------------------------------------------------+|
//! || s | s | .   || n | n | p || data (encoded data shred data)                                ||
//! || i | h | .   || u | u | o ||                                                               ||
//! || g | r | .   || m | m | s ||                                                               ||
//! || n | e |     ||   |   | i ||                                                               ||
//! || a | d |     || d | c | t ||                                                               ||
//! || t |   |     ||   |   | i ||                                                               ||
//! || u | t |     || s | s | o ||                                                               ||
//! || r | y |     || h | h | n ||                                                               ||
//! || e | p |     || r | r |   ||                                                               ||
//! ||   | e |     || e | e |   ||                                                               ||
//! ||   |   |     || d | d |   ||                                                               ||
//! |+---+---+---  |+---+---+---+|+--------------------------------------------------------------+|
//! +---------------------------------------------------------------------------------------------+
//!
//! Notes:
//! a) Coding shreds encode entire data shreds: both of the headers AND the payload.
//! b) Coding shreds require their own headers for identification and etc.
//! c) The erasure algorithm requires data shred and coding shred bytestreams to be equal in length.
//!
//! So, given a) - c), we must restrict data shred's payload length such that the entire coding
//! payload can fit into one coding shred / packet.

pub(crate) use shred_data::ShredData;
pub use {
    self::stats::{ProcessShredsStats, ShredFetchStats},
    crate::shredder::Shredder,
};
use {
    self::{shred_code::ShredCode, traits::Shred as _},
    crate::blockstore::MAX_DATA_SHREDS_PER_SLOT,
    bitflags::bitflags,
    num_enum::{IntoPrimitive, TryFromPrimitive},
    serde::{Deserialize, Serialize},
    solana_entry::entry::{create_ticks, Entry},
    solana_perf::packet::{deserialize_from_with_limit, Packet},
    solana_runtime::bank::Bank,
    solana_sdk::{
        clock::Slot,
        feature_set,
        hash::{hashv, Hash},
        pubkey::Pubkey,
        signature::{Keypair, Signature, Signer},
    },
    static_assertions::const_assert_eq,
    std::fmt::Debug,
    thiserror::Error,
};

mod common;
mod legacy;
mod shred_code;
mod shred_data;
mod stats;
mod traits;

pub type Nonce = u32;
pub const SIZE_OF_NONCE: usize = 4;

/// The following constants are computed by hand, and hardcoded.
/// `test_shred_constants` ensures that the values are correct.
/// Constants are used over lazy_static for performance reasons.
const SIZE_OF_COMMON_SHRED_HEADER: usize = 83;
const SIZE_OF_DATA_SHRED_HEADERS: usize = 88;
const SIZE_OF_CODING_SHRED_HEADERS: usize = 89;
const SIZE_OF_SIGNATURE: usize = 64;
const SIZE_OF_SHRED_VARIANT: usize = 1;
const SIZE_OF_SHRED_SLOT: usize = 8;
const SIZE_OF_SHRED_INDEX: usize = 4;

const OFFSET_OF_SHRED_VARIANT: usize = SIZE_OF_SIGNATURE;
const OFFSET_OF_SHRED_SLOT: usize = SIZE_OF_SIGNATURE + SIZE_OF_SHRED_VARIANT;
const OFFSET_OF_SHRED_INDEX: usize = OFFSET_OF_SHRED_SLOT + SIZE_OF_SHRED_SLOT;

pub const MAX_DATA_SHREDS_PER_FEC_BLOCK: u32 = 32;

// For legacy tests and benchmarks.
const_assert_eq!(LEGACY_SHRED_DATA_CAPACITY, 1051);
pub const LEGACY_SHRED_DATA_CAPACITY: usize = legacy::ShredData::CAPACITY;

// LAST_SHRED_IN_SLOT also implies DATA_COMPLETE_SHRED.
// So it cannot be LAST_SHRED_IN_SLOT if not also DATA_COMPLETE_SHRED.
bitflags! {
    #[derive(Default, Serialize, Deserialize)]
    pub struct ShredFlags:u8 {
        const SHRED_TICK_REFERENCE_MASK = 0b0011_1111;
        const DATA_COMPLETE_SHRED       = 0b0100_0000;
        const LAST_SHRED_IN_SLOT        = 0b1100_0000;
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    BincodeError(#[from] bincode::Error),
    #[error(transparent)]
    ErasureError(#[from] reed_solomon_erasure::Error),
    #[error("Invalid data shred index: {0}")]
    InvalidDataShredIndex(/*shred index:*/ u32),
    #[error("Invalid data size: {size}, payload: {payload}")]
    InvalidDataSize { size: u16, payload: usize },
    #[error("Invalid erasure shard index: {0:?}")]
    InvalidErasureShardIndex(/*headers:*/ Box<dyn Debug>),
    #[error("Invalid num coding shreds: {0}")]
    InvalidNumCodingShreds(u16),
    #[error("Invalid parent_offset: {parent_offset}, slot: {slot}")]
    InvalidParentOffset { slot: Slot, parent_offset: u16 },
    #[error("Invalid parent slot: {parent_slot}, slot: {slot}")]
    InvalidParentSlot { slot: Slot, parent_slot: Slot },
    #[error("Invalid payload size: {0}")]
    InvalidPayloadSize(/*payload size:*/ usize),
    #[error("Invalid shred flags: {0}")]
    InvalidShredFlags(u8),
    #[error("Invalid shred type")]
    InvalidShredType,
    #[error("Invalid shred variant")]
    InvalidShredVariant,
}

#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    PartialEq,
    AbiEnumVisitor,
    AbiExample,
    Deserialize,
    IntoPrimitive,
    Serialize,
    TryFromPrimitive,
)]
#[serde(into = "u8", try_from = "u8")]
pub enum ShredType {
    Data = 0b1010_0101,
    Code = 0b0101_1010,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(into = "u8", try_from = "u8")]
enum ShredVariant {
    LegacyCode, // 0b0101_1010
    LegacyData, // 0b1010_0101
}

/// A common header that is present in data and code shred headers
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct ShredCommonHeader {
    signature: Signature,
    shred_variant: ShredVariant,
    slot: Slot,
    index: u32,
    version: u16,
    fec_set_index: u32,
}

/// The data shred header has parent offset and flags
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct DataShredHeader {
    parent_offset: u16,
    flags: ShredFlags,
    size: u16, // common shred header + data shred header + data
}

/// The coding shred header has FEC information
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct CodingShredHeader {
    num_data_shreds: u16,
    num_coding_shreds: u16,
    position: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shred {
    ShredCode(ShredCode),
    ShredData(ShredData),
}

/// Tuple which uniquely identifies a shred should it exists.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ShredId(Slot, /*shred index:*/ u32, ShredType);

impl ShredId {
    pub(crate) fn new(slot: Slot, index: u32, shred_type: ShredType) -> ShredId {
        ShredId(slot, index, shred_type)
    }

    pub(crate) fn unwrap(&self) -> (Slot, /*shred index:*/ u32, ShredType) {
        (self.0, self.1, self.2)
    }
}

/// Tuple which identifies erasure coding set that the shred belongs to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ErasureSetId(Slot, /*fec_set_index:*/ u32);

impl ErasureSetId {
    pub(crate) fn slot(&self) -> Slot {
        self.0
    }

    // Storage key for ErasureMeta in blockstore db.
    pub(crate) fn store_key(&self) -> (Slot, /*fec_set_index:*/ u64) {
        (self.0, u64::from(self.1))
    }
}

macro_rules! dispatch {
    ($vis:vis fn $name:ident(&self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(&self $(, $arg:$ty)?) $(-> $out)? {
            match self {
                Self::ShredCode(shred) => shred.$name($($arg, )?),
                Self::ShredData(shred) => shred.$name($($arg, )?),
            }
        }
    };
    ($vis:vis fn $name:ident(self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(self $(, $arg:$ty)?) $(-> $out)? {
            match self {
                Self::ShredCode(shred) => shred.$name($($arg, )?),
                Self::ShredData(shred) => shred.$name($($arg, )?),
            }
        }
    };
    ($vis:vis fn $name:ident(&mut self $(, $arg:ident : $ty:ty)?) $(-> $out:ty)?) => {
        #[inline]
        $vis fn $name(&mut self $(, $arg:$ty)?) $(-> $out)? {
            match self {
                Self::ShredCode(shred) => shred.$name($($arg, )?),
                Self::ShredData(shred) => shred.$name($($arg, )?),
            }
        }
    }
}

impl Shred {
    dispatch!(fn common_header(&self) -> &ShredCommonHeader);
    dispatch!(fn set_signature(&mut self, signature: Signature));
    dispatch!(fn signed_message(&self) -> &[u8]);

    // Returns the portion of the shred's payload which is erasure coded.
    dispatch!(pub(crate) fn erasure_shard(self) -> Result<Vec<u8>, Error>);
    // Like Shred::erasure_shard but returning a slice.
    dispatch!(pub(crate) fn erasure_shard_as_slice(&self) -> Result<&[u8], Error>);
    // Returns the shard index within the erasure coding set.
    dispatch!(pub(crate) fn erasure_shard_index(&self) -> Result<usize, Error>);

    dispatch!(pub fn into_payload(self) -> Vec<u8>);
    dispatch!(pub fn payload(&self) -> &Vec<u8>);
    dispatch!(pub fn sanitize(&self) -> Result<(), Error>);

    // Only for tests.
    dispatch!(pub fn set_index(&mut self, index: u32));
    dispatch!(pub fn set_slot(&mut self, slot: Slot));

    pub fn copy_to_packet(&self, packet: &mut Packet) {
        let payload = self.payload();
        let size = payload.len();
        packet.buffer_mut()[..size].copy_from_slice(&payload[..]);
        packet.meta.size = size;
    }

    // TODO: Should this sanitize output?
    pub fn new_from_data(
        slot: Slot,
        index: u32,
        parent_offset: u16,
        data: &[u8],
        flags: ShredFlags,
        reference_tick: u8,
        version: u16,
        fec_set_index: u32,
    ) -> Self {
        Self::from(ShredData::new_from_data(
            slot,
            index,
            parent_offset,
            data,
            flags,
            reference_tick,
            version,
            fec_set_index,
        ))
    }

    pub fn new_from_serialized_shred(shred: Vec<u8>) -> Result<Self, Error> {
        Ok(match layout::get_shred_variant(&shred)? {
            ShredVariant::LegacyCode => {
                let shred = legacy::ShredCode::from_payload(shred)?;
                Self::from(ShredCode::from(shred))
            }
            ShredVariant::LegacyData => {
                let shred = legacy::ShredData::from_payload(shred)?;
                Self::from(ShredData::from(shred))
            }
        })
    }

    pub fn new_from_parity_shard(
        slot: Slot,
        index: u32,
        parity_shard: &[u8],
        fec_set_index: u32,
        num_data_shreds: u16,
        num_coding_shreds: u16,
        position: u16,
        version: u16,
    ) -> Self {
        Self::from(ShredCode::new_from_parity_shard(
            slot,
            index,
            parity_shard,
            fec_set_index,
            num_data_shreds,
            num_coding_shreds,
            position,
            version,
        ))
    }

    /// Unique identifier for each shred.
    pub fn id(&self) -> ShredId {
        ShredId(self.slot(), self.index(), self.shred_type())
    }

    pub fn slot(&self) -> Slot {
        self.common_header().slot
    }

    pub fn parent(&self) -> Result<Slot, Error> {
        match self {
            Self::ShredCode(_) => Err(Error::InvalidShredType),
            Self::ShredData(shred) => shred.parent(),
        }
    }

    pub fn index(&self) -> u32 {
        self.common_header().index
    }

    pub(crate) fn data(&self) -> Result<&[u8], Error> {
        match self {
            Self::ShredCode(_) => Err(Error::InvalidShredType),
            Self::ShredData(shred) => shred.data(),
        }
    }

    // Possibly trimmed payload;
    // Should only be used when storing shreds to blockstore.
    pub(crate) fn bytes_to_store(&self) -> &[u8] {
        match self {
            Self::ShredCode(shred) => shred.payload(),
            Self::ShredData(shred) => shred.bytes_to_store(),
        }
    }

    pub fn fec_set_index(&self) -> u32 {
        self.common_header().fec_set_index
    }

    pub(crate) fn first_coding_index(&self) -> Option<u32> {
        match self {
            Self::ShredCode(shred) => shred.first_coding_index(),
            Self::ShredData(_) => None,
        }
    }

    pub fn version(&self) -> u16 {
        self.common_header().version
    }

    // Identifier for the erasure coding set that the shred belongs to.
    pub(crate) fn erasure_set(&self) -> ErasureSetId {
        ErasureSetId(self.slot(), self.fec_set_index())
    }

    pub fn signature(&self) -> Signature {
        self.common_header().signature
    }

    pub fn sign(&mut self, keypair: &Keypair) {
        let signature = keypair.sign_message(self.signed_message());
        self.set_signature(signature);
    }

    pub fn seed(&self, leader_pubkey: Pubkey, root_bank: &Bank) -> [u8; 32] {
        if add_shred_type_to_shred_seed(self.slot(), root_bank) {
            hashv(&[
                &self.slot().to_le_bytes(),
                &u8::from(self.shred_type()).to_le_bytes(),
                &self.index().to_le_bytes(),
                &leader_pubkey.to_bytes(),
            ])
        } else {
            hashv(&[
                &self.slot().to_le_bytes(),
                &self.index().to_le_bytes(),
                &leader_pubkey.to_bytes(),
            ])
        }
        .to_bytes()
    }

    #[inline]
    pub fn shred_type(&self) -> ShredType {
        ShredType::from(self.common_header().shred_variant)
    }

    pub fn is_data(&self) -> bool {
        self.shred_type() == ShredType::Data
    }
    pub fn is_code(&self) -> bool {
        self.shred_type() == ShredType::Code
    }

    pub fn last_in_slot(&self) -> bool {
        match self {
            Self::ShredCode(_) => false,
            Self::ShredData(shred) => shred.last_in_slot(),
        }
    }

    /// This is not a safe function. It only changes the meta information.
    /// Use this only for test code which doesn't care about actual shred
    pub fn set_last_in_slot(&mut self) {
        match self {
            Self::ShredCode(_) => (),
            Self::ShredData(shred) => shred.set_last_in_slot(),
        }
    }

    pub fn data_complete(&self) -> bool {
        match self {
            Self::ShredCode(_) => false,
            Self::ShredData(shred) => shred.data_complete(),
        }
    }

    pub(crate) fn reference_tick(&self) -> u8 {
        match self {
            Self::ShredCode(_) => ShredFlags::SHRED_TICK_REFERENCE_MASK.bits(),
            Self::ShredData(shred) => shred.reference_tick(),
        }
    }

    pub fn verify(&self, pubkey: &Pubkey) -> bool {
        let message = self.signed_message();
        self.signature().verify(pubkey.as_ref(), message)
    }

    // Returns true if the erasure coding of the two shreds mismatch.
    pub(crate) fn erasure_mismatch(&self, other: &Self) -> Result<bool, Error> {
        match (self, other) {
            (Self::ShredCode(shred), Self::ShredCode(other)) => Ok(shred.erasure_mismatch(other)),
            _ => Err(Error::InvalidShredType),
        }
    }

    pub(crate) fn num_data_shreds(&self) -> Result<u16, Error> {
        match self {
            Self::ShredCode(shred) => Ok(shred.num_data_shreds()),
            Self::ShredData(_) => Err(Error::InvalidShredType),
        }
    }

    pub(crate) fn num_coding_shreds(&self) -> Result<u16, Error> {
        match self {
            Self::ShredCode(shred) => Ok(shred.num_coding_shreds()),
            Self::ShredData(_) => Err(Error::InvalidShredType),
        }
    }
}

// Helper methods to extract pieces of the shred from the payload
// without deserializing the entire payload.
pub mod layout {
    use {super::*, std::ops::Range};

    fn get_shred_size(packet: &Packet) -> Option<usize> {
        let size = packet.data(..)?.len();
        if packet.meta.repair() {
            size.checked_sub(SIZE_OF_NONCE)
        } else {
            Some(size)
        }
    }

    pub fn get_shred(packet: &Packet) -> Option<&[u8]> {
        let size = get_shred_size(packet)?;
        let shred = packet.data(..size)?;
        // Should at least have a signature.
        (size >= SIZE_OF_SIGNATURE).then(|| shred)
    }

    pub(crate) fn get_signature(shred: &[u8]) -> Option<Signature> {
        Some(Signature::new(shred.get(..SIZE_OF_SIGNATURE)?))
    }

    pub(crate) const fn get_signature_range() -> Range<usize> {
        0..SIZE_OF_SIGNATURE
    }

    pub(super) fn get_shred_variant(shred: &[u8]) -> Result<ShredVariant, Error> {
        let shred_variant = match shred.get(OFFSET_OF_SHRED_VARIANT) {
            None => return Err(Error::InvalidPayloadSize(shred.len())),
            Some(shred_variant) => *shred_variant,
        };
        ShredVariant::try_from(shred_variant).map_err(|_| Error::InvalidShredVariant)
    }

    pub(super) fn get_shred_type(shred: &[u8]) -> Result<ShredType, Error> {
        let shred_variant = get_shred_variant(shred)?;
        Ok(ShredType::from(shred_variant))
    }

    pub fn get_slot(shred: &[u8]) -> Option<Slot> {
        deserialize_from_with_limit(shred.get(OFFSET_OF_SHRED_SLOT..)?).ok()
    }

    pub(super) fn get_index(shred: &[u8]) -> Option<u32> {
        deserialize_from_with_limit(shred.get(OFFSET_OF_SHRED_INDEX..)?).ok()
    }

    // Returns slice range of the shred payload which is signed.
    pub(crate) fn get_signed_message_range(shred: &[u8]) -> Option<Range<usize>> {
        let range = match get_shred_variant(shred).ok()? {
            ShredVariant::LegacyCode | ShredVariant::LegacyData => legacy::SIGNED_MESSAGE_RANGE,
        };
        (shred.len() <= range.end).then(|| range)
    }

    pub(crate) fn get_reference_tick(shred: &[u8]) -> Result<u8, Error> {
        const SIZE_OF_PARENT_OFFSET: usize = std::mem::size_of::<u16>();
        const OFFSET_OF_SHRED_FLAGS: usize = SIZE_OF_COMMON_SHRED_HEADER + SIZE_OF_PARENT_OFFSET;
        if get_shred_type(shred)? != ShredType::Data {
            return Err(Error::InvalidShredType);
        }
        let flags = match shred.get(OFFSET_OF_SHRED_FLAGS) {
            None => return Err(Error::InvalidPayloadSize(shred.len())),
            Some(flags) => flags,
        };
        Ok(flags & ShredFlags::SHRED_TICK_REFERENCE_MASK.bits())
    }
}

impl From<ShredCode> for Shred {
    fn from(shred: ShredCode) -> Self {
        Self::ShredCode(shred)
    }
}

impl From<ShredData> for Shred {
    fn from(shred: ShredData) -> Self {
        Self::ShredData(shred)
    }
}

impl From<ShredVariant> for ShredType {
    #[inline]
    fn from(shred_variant: ShredVariant) -> Self {
        match shred_variant {
            ShredVariant::LegacyCode => ShredType::Code,
            ShredVariant::LegacyData => ShredType::Data,
        }
    }
}

impl From<ShredVariant> for u8 {
    fn from(shred_variant: ShredVariant) -> u8 {
        match shred_variant {
            ShredVariant::LegacyCode => u8::from(ShredType::Code),
            ShredVariant::LegacyData => u8::from(ShredType::Data),
        }
    }
}

impl TryFrom<u8> for ShredVariant {
    type Error = Error;
    fn try_from(shred_variant: u8) -> Result<Self, Self::Error> {
        if shred_variant == u8::from(ShredType::Code) {
            Ok(ShredVariant::LegacyCode)
        } else if shred_variant == u8::from(ShredType::Data) {
            Ok(ShredVariant::LegacyData)
        } else {
            Err(Error::InvalidShredVariant)
        }
    }
}

// Get slot, index, and type from a packet with partial deserialize
pub fn get_shred_slot_index_type(
    packet: &Packet,
    stats: &mut ShredFetchStats,
) -> Option<(Slot, u32, ShredType)> {
    let shred = match layout::get_shred(packet) {
        None => {
            stats.index_overrun += 1;
            return None;
        }
        Some(shred) => shred,
    };
    if OFFSET_OF_SHRED_INDEX + SIZE_OF_SHRED_INDEX > shred.len() {
        stats.index_overrun += 1;
        return None;
    }
    let shred_type = match layout::get_shred_type(shred) {
        Ok(shred_type) => shred_type,
        Err(_) => {
            stats.bad_shred_type += 1;
            return None;
        }
    };
    let slot = match layout::get_slot(shred) {
        Some(slot) => slot,
        None => {
            stats.slot_bad_deserialize += 1;
            return None;
        }
    };
    let index = match layout::get_index(shred) {
        Some(index) => index,
        None => {
            stats.index_bad_deserialize += 1;
            return None;
        }
    };
    if index >= MAX_DATA_SHREDS_PER_SLOT as u32 {
        stats.index_out_of_bounds += 1;
        return None;
    }
    Some((slot, index, shred_type))
}

pub fn max_ticks_per_n_shreds(num_shreds: u64, shred_data_size: Option<usize>) -> u64 {
    let ticks = create_ticks(1, 0, Hash::default());
    max_entries_per_n_shred(&ticks[0], num_shreds, shred_data_size)
}

pub fn max_entries_per_n_shred(
    entry: &Entry,
    num_shreds: u64,
    shred_data_size: Option<usize>,
) -> u64 {
    let data_buffer_size = ShredData::capacity().unwrap();
    let shred_data_size = shred_data_size.unwrap_or(data_buffer_size) as u64;
    let vec_size = bincode::serialized_size(&vec![entry]).unwrap();
    let entry_size = bincode::serialized_size(entry).unwrap();
    let count_size = vec_size - entry_size;

    (shred_data_size * num_shreds - count_size) / entry_size
}

pub fn verify_test_data_shred(
    shred: &Shred,
    index: u32,
    slot: Slot,
    parent: Slot,
    pk: &Pubkey,
    verify: bool,
    is_last_in_slot: bool,
    is_last_data: bool,
) {
    shred.sanitize().unwrap();
    assert!(shred.is_data());
    assert_eq!(shred.index(), index);
    assert_eq!(shred.slot(), slot);
    assert_eq!(shred.parent().unwrap(), parent);
    assert_eq!(verify, shred.verify(pk));
    if is_last_in_slot {
        assert!(shred.last_in_slot());
    } else {
        assert!(!shred.last_in_slot());
    }
    if is_last_data {
        assert!(shred.data_complete());
    } else {
        assert!(!shred.data_complete());
    }
}

fn add_shred_type_to_shred_seed(shred_slot: Slot, bank: &Bank) -> bool {
    let feature_slot = bank
        .feature_set
        .activated_slot(&feature_set::add_shred_type_to_shred_seed::id());
    match feature_slot {
        None => false,
        Some(feature_slot) => {
            let epoch_schedule = bank.epoch_schedule();
            let feature_epoch = epoch_schedule.get_epoch(feature_slot);
            let shred_epoch = epoch_schedule.get_epoch(shred_slot);
            feature_epoch < shred_epoch
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        bincode::serialized_size,
        matches::assert_matches,
        rand::Rng,
        rand_chacha::{rand_core::SeedableRng, ChaChaRng},
        solana_sdk::{shred_version, signature::Signer},
    };

    fn bs58_decode<T: AsRef<[u8]>>(data: T) -> Vec<u8> {
        bs58::decode(data).into_vec().unwrap()
    }

    #[test]
    fn test_shred_constants() {
        let common_header = ShredCommonHeader {
            signature: Signature::default(),
            shred_variant: ShredVariant::LegacyCode,
            slot: Slot::MAX,
            index: u32::MAX,
            version: u16::MAX,
            fec_set_index: u32::MAX,
        };
        let data_shred_header = DataShredHeader {
            parent_offset: u16::MAX,
            flags: ShredFlags::all(),
            size: u16::MAX,
        };
        let coding_shred_header = CodingShredHeader {
            num_data_shreds: u16::MAX,
            num_coding_shreds: u16::MAX,
            position: u16::MAX,
        };
        assert_eq!(
            SIZE_OF_COMMON_SHRED_HEADER,
            serialized_size(&common_header).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_CODING_SHRED_HEADERS - SIZE_OF_COMMON_SHRED_HEADER,
            serialized_size(&coding_shred_header).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_DATA_SHRED_HEADERS - SIZE_OF_COMMON_SHRED_HEADER,
            serialized_size(&data_shred_header).unwrap() as usize
        );
        let data_shred_header_with_size = DataShredHeader {
            size: 1000,
            ..data_shred_header
        };
        assert_eq!(
            SIZE_OF_DATA_SHRED_HEADERS - SIZE_OF_COMMON_SHRED_HEADER,
            serialized_size(&data_shred_header_with_size).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_SIGNATURE,
            bincode::serialized_size(&Signature::default()).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_SHRED_VARIANT,
            bincode::serialized_size(&ShredVariant::LegacyCode).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_SHRED_SLOT,
            bincode::serialized_size(&Slot::default()).unwrap() as usize
        );
        assert_eq!(
            SIZE_OF_SHRED_INDEX,
            bincode::serialized_size(&common_header.index).unwrap() as usize
        );
    }

    #[test]
    fn test_version_from_hash() {
        let hash = [
            0xa5u8, 0xa5, 0x5a, 0x5a, 0xa5, 0xa5, 0x5a, 0x5a, 0xa5, 0xa5, 0x5a, 0x5a, 0xa5, 0xa5,
            0x5a, 0x5a, 0xa5, 0xa5, 0x5a, 0x5a, 0xa5, 0xa5, 0x5a, 0x5a, 0xa5, 0xa5, 0x5a, 0x5a,
            0xa5, 0xa5, 0x5a, 0x5a,
        ];
        let version = shred_version::version_from_hash(&Hash::new(&hash));
        assert_eq!(version, 1);
        let hash = [
            0xa5u8, 0xa5, 0x5a, 0x5a, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let version = shred_version::version_from_hash(&Hash::new(&hash));
        assert_eq!(version, 0xffff);
        let hash = [
            0xa5u8, 0xa5, 0x5a, 0x5a, 0xa5, 0xa5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let version = shred_version::version_from_hash(&Hash::new(&hash));
        assert_eq!(version, 0x5a5b);
    }

    #[test]
    fn test_invalid_parent_offset() {
        let shred = Shred::new_from_data(10, 0, 1000, &[1, 2, 3], ShredFlags::empty(), 0, 1, 0);
        let mut packet = Packet::default();
        shred.copy_to_packet(&mut packet);
        let shred_res = Shred::new_from_serialized_shred(packet.data(..).unwrap().to_vec());
        assert_matches!(
            shred.parent(),
            Err(Error::InvalidParentOffset {
                slot: 10,
                parent_offset: 1000
            })
        );
        assert_matches!(
            shred_res,
            Err(Error::InvalidParentOffset {
                slot: 10,
                parent_offset: 1000
            })
        );
    }

    #[test]
    fn test_shred_offsets() {
        solana_logger::setup();
        let mut packet = Packet::default();
        let shred = Shred::new_from_data(1, 3, 0, &[], ShredFlags::LAST_SHRED_IN_SLOT, 0, 0, 0);
        shred.copy_to_packet(&mut packet);
        let mut stats = ShredFetchStats::default();
        let ret = get_shred_slot_index_type(&packet, &mut stats);
        assert_eq!(Some((1, 3, ShredType::Data)), ret);
        assert_eq!(stats, ShredFetchStats::default());

        packet.meta.size = OFFSET_OF_SHRED_VARIANT;
        assert_eq!(None, get_shred_slot_index_type(&packet, &mut stats));
        assert_eq!(stats.index_overrun, 1);

        packet.meta.size = OFFSET_OF_SHRED_INDEX;
        assert_eq!(None, get_shred_slot_index_type(&packet, &mut stats));
        assert_eq!(stats.index_overrun, 2);

        packet.meta.size = OFFSET_OF_SHRED_INDEX + 1;
        assert_eq!(None, get_shred_slot_index_type(&packet, &mut stats));
        assert_eq!(stats.index_overrun, 3);

        packet.meta.size = OFFSET_OF_SHRED_INDEX + SIZE_OF_SHRED_INDEX - 1;
        assert_eq!(None, get_shred_slot_index_type(&packet, &mut stats));
        assert_eq!(stats.index_overrun, 4);

        packet.meta.size = OFFSET_OF_SHRED_INDEX + SIZE_OF_SHRED_INDEX;
        assert_eq!(
            Some((1, 3, ShredType::Data)),
            get_shred_slot_index_type(&packet, &mut stats)
        );
        assert_eq!(stats.index_overrun, 4);

        let shred = Shred::new_from_parity_shard(
            8,   // slot
            2,   // index
            &[], // parity_shard
            10,  // fec_set_index
            30,  // num_data
            4,   // num_code
            1,   // position
            200, // version
        );
        shred.copy_to_packet(&mut packet);
        assert_eq!(
            Some((8, 2, ShredType::Code)),
            get_shred_slot_index_type(&packet, &mut stats)
        );

        let shred = Shred::new_from_data(
            1,
            std::u32::MAX - 10,
            0,
            &[],
            ShredFlags::LAST_SHRED_IN_SLOT,
            0,
            0,
            0,
        );
        shred.copy_to_packet(&mut packet);
        assert_eq!(None, get_shred_slot_index_type(&packet, &mut stats));
        assert_eq!(1, stats.index_out_of_bounds);

        let shred = Shred::new_from_parity_shard(
            8,   // slot
            2,   // index
            &[], // parity_shard
            10,  // fec_set_index
            30,  // num_data_shreds
            4,   // num_coding_shreds
            3,   // position
            200, // version
        );
        shred.copy_to_packet(&mut packet);
        packet.buffer_mut()[OFFSET_OF_SHRED_VARIANT] = u8::MAX;

        assert_eq!(None, get_shred_slot_index_type(&packet, &mut stats));
        assert_eq!(1, stats.bad_shred_type);
    }

    // Asserts that ShredType is backward compatible with u8.
    #[test]
    fn test_shred_type_compat() {
        assert_eq!(std::mem::size_of::<ShredType>(), std::mem::size_of::<u8>());
        assert_matches!(ShredType::try_from(0u8), Err(_));
        assert_matches!(ShredType::try_from(1u8), Err(_));
        assert_matches!(bincode::deserialize::<ShredType>(&[0u8]), Err(_));
        assert_matches!(bincode::deserialize::<ShredType>(&[1u8]), Err(_));
        // data shred
        assert_eq!(ShredType::Data as u8, 0b1010_0101);
        assert_eq!(u8::from(ShredType::Data), 0b1010_0101);
        assert_eq!(ShredType::try_from(0b1010_0101), Ok(ShredType::Data));
        let buf = bincode::serialize(&ShredType::Data).unwrap();
        assert_eq!(buf, vec![0b1010_0101]);
        assert_matches!(
            bincode::deserialize::<ShredType>(&[0b1010_0101]),
            Ok(ShredType::Data)
        );
        // coding shred
        assert_eq!(ShredType::Code as u8, 0b0101_1010);
        assert_eq!(u8::from(ShredType::Code), 0b0101_1010);
        assert_eq!(ShredType::try_from(0b0101_1010), Ok(ShredType::Code));
        let buf = bincode::serialize(&ShredType::Code).unwrap();
        assert_eq!(buf, vec![0b0101_1010]);
        assert_matches!(
            bincode::deserialize::<ShredType>(&[0b0101_1010]),
            Ok(ShredType::Code)
        );
    }

    #[test]
    fn test_shred_variant_compat() {
        assert_matches!(ShredVariant::try_from(0u8), Err(_));
        assert_matches!(ShredVariant::try_from(1u8), Err(_));
        assert_matches!(ShredVariant::try_from(0b0101_0000), Err(_));
        assert_matches!(ShredVariant::try_from(0b1010_0000), Err(_));
        assert_matches!(bincode::deserialize::<ShredVariant>(&[0b0101_0000]), Err(_));
        assert_matches!(bincode::deserialize::<ShredVariant>(&[0b1010_0000]), Err(_));
        // Legacy coding shred.
        assert_eq!(u8::from(ShredVariant::LegacyCode), 0b0101_1010);
        assert_eq!(ShredType::from(ShredVariant::LegacyCode), ShredType::Code);
        assert_matches!(
            ShredVariant::try_from(0b0101_1010),
            Ok(ShredVariant::LegacyCode)
        );
        let buf = bincode::serialize(&ShredVariant::LegacyCode).unwrap();
        assert_eq!(buf, vec![0b0101_1010]);
        assert_matches!(
            bincode::deserialize::<ShredVariant>(&[0b0101_1010]),
            Ok(ShredVariant::LegacyCode)
        );
        // Legacy data shred.
        assert_eq!(u8::from(ShredVariant::LegacyData), 0b1010_0101);
        assert_eq!(ShredType::from(ShredVariant::LegacyData), ShredType::Data);
        assert_matches!(
            ShredVariant::try_from(0b1010_0101),
            Ok(ShredVariant::LegacyData)
        );
        let buf = bincode::serialize(&ShredVariant::LegacyData).unwrap();
        assert_eq!(buf, vec![0b1010_0101]);
        assert_matches!(
            bincode::deserialize::<ShredVariant>(&[0b1010_0101]),
            Ok(ShredVariant::LegacyData)
        );
    }

    #[test]
    fn test_serde_compat_shred_data() {
        const SEED: &str = "6qG9NGWEtoTugS4Zgs46u8zTccEJuRHtrNMiUayLHCxt";
        const PAYLOAD: &str = "hNX8YgJCQwSFGJkZ6qZLiepwPjpctC9UCsMD1SNNQurBXv\
        rm7KKfLmPRMM9CpWHt6MsJuEWpDXLGwH9qdziJzGKhBMfYH63avcchjdaUiMqzVip7cUD\
        kqZ9zZJMrHCCUDnxxKMupsJWKroUSjKeo7hrug2KfHah85VckXpRna4R9QpH7tf2WVBTD\
        M4m3EerctsEQs8eZaTRxzTVkhtJYdNf74KZbH58dc3Yn2qUxF1mexWoPS6L5oZBatx";
        let mut rng = {
            let seed = <[u8; 32]>::try_from(bs58_decode(SEED)).unwrap();
            ChaChaRng::from_seed(seed)
        };
        let mut data = [0u8; legacy::ShredData::CAPACITY];
        rng.fill(&mut data[..]);
        let keypair = Keypair::generate(&mut rng);
        let mut shred = Shred::new_from_data(
            141939602, // slot
            28685,     // index
            36390,     // parent_offset
            &data,     // data
            ShredFlags::LAST_SHRED_IN_SLOT,
            37,    // reference_tick
            45189, // version
            28657, // fec_set_index
        );
        shred.sign(&keypair);
        assert!(shred.verify(&keypair.pubkey()));
        assert_matches!(shred.sanitize(), Ok(()));
        let mut payload = bs58_decode(PAYLOAD);
        payload.extend({
            let skip = payload.len() - SIZE_OF_DATA_SHRED_HEADERS;
            data.iter().skip(skip).copied()
        });
        let mut packet = Packet::default();
        packet.buffer_mut()[..payload.len()].copy_from_slice(&payload);
        packet.meta.size = payload.len();
        assert_eq!(shred.bytes_to_store(), payload);
        assert_eq!(shred, Shred::new_from_serialized_shred(payload).unwrap());
        assert_eq!(
            shred.reference_tick(),
            layout::get_reference_tick(packet.data(..).unwrap()).unwrap()
        );
        assert_eq!(
            layout::get_slot(packet.data(..).unwrap()),
            Some(shred.slot())
        );
        assert_eq!(
            get_shred_slot_index_type(&packet, &mut ShredFetchStats::default()),
            Some((shred.slot(), shred.index(), shred.shred_type()))
        );
    }

    #[test]
    fn test_serde_compat_shred_data_empty() {
        const SEED: &str = "E3M5hm8yAEB7iPhQxFypAkLqxNeZCTuGBDMa8Jdrghoo";
        const PAYLOAD: &str = "nRNFVBEsV9FEM5KfmsCXJsgELRSkCV55drTavdy5aZPnsp\
        B8WvsgY99ZuNHDnwkrqe6Lx7ARVmercwugR5HwDcLA9ivKMypk9PNucDPLs67TXWy6k9R\
        ozKmy";
        let mut rng = {
            let seed = <[u8; 32]>::try_from(bs58_decode(SEED)).unwrap();
            ChaChaRng::from_seed(seed)
        };
        let keypair = Keypair::generate(&mut rng);
        let mut shred = Shred::new_from_data(
            142076266, // slot
            21443,     // index
            51279,     // parent_offset
            &[],       // data
            ShredFlags::DATA_COMPLETE_SHRED,
            49,    // reference_tick
            59445, // version
            21414, // fec_set_index
        );
        shred.sign(&keypair);
        assert!(shred.verify(&keypair.pubkey()));
        assert_matches!(shred.sanitize(), Ok(()));
        let payload = bs58_decode(PAYLOAD);
        let mut packet = Packet::default();
        packet.buffer_mut()[..payload.len()].copy_from_slice(&payload);
        packet.meta.size = payload.len();
        assert_eq!(shred.bytes_to_store(), payload);
        assert_eq!(shred, Shred::new_from_serialized_shred(payload).unwrap());
        assert_eq!(
            shred.reference_tick(),
            layout::get_reference_tick(packet.data(..).unwrap()).unwrap()
        );
        assert_eq!(
            layout::get_slot(packet.data(..).unwrap()),
            Some(shred.slot())
        );
        assert_eq!(
            get_shred_slot_index_type(&packet, &mut ShredFetchStats::default()),
            Some((shred.slot(), shred.index(), shred.shred_type()))
        );
    }

    #[test]
    fn test_serde_compat_shred_code() {
        const SEED: &str = "4jfjh3UZVyaEgvyG9oQmNyFY9yHDmbeH9eUhnBKkrcrN";
        const PAYLOAD: &str = "3xGsXwzkPpLFuKwbbfKMUxt1B6VqQPzbvvAkxRNCX9kNEP\
        sa2VifwGBtFuNm3CWXdmQizDz5vJjDHu6ZqqaBCSfrHurag87qAXwTtjNPhZzKEew5pLc\
        aY6cooiAch2vpfixNYSDjnirozje5cmUtGuYs1asXwsAKSN3QdWHz3XGParWkZeUMAzRV\
        1UPEDZ7vETKbxeNixKbzZzo47Lakh3C35hS74ocfj23CWoW1JpkETkXjUpXcfcv6cS";
        let mut rng = {
            let seed = <[u8; 32]>::try_from(bs58_decode(SEED)).unwrap();
            ChaChaRng::from_seed(seed)
        };
        let mut parity_shard = vec![0u8; legacy::SIZE_OF_ERASURE_ENCODED_SLICE];
        rng.fill(&mut parity_shard[..]);
        let keypair = Keypair::generate(&mut rng);
        let mut shred = Shred::new_from_parity_shard(
            141945197, // slot
            23418,     // index
            &parity_shard,
            21259, // fec_set_index
            32,    // num_data_shreds
            58,    // num_coding_shreds
            43,    // position
            47298, // version
        );
        shred.sign(&keypair);
        assert!(shred.verify(&keypair.pubkey()));
        assert_matches!(shred.sanitize(), Ok(()));
        let mut payload = bs58_decode(PAYLOAD);
        payload.extend({
            let skip = payload.len() - SIZE_OF_CODING_SHRED_HEADERS;
            parity_shard.iter().skip(skip).copied()
        });
        let mut packet = Packet::default();
        packet.buffer_mut()[..payload.len()].copy_from_slice(&payload);
        packet.meta.size = payload.len();
        assert_eq!(shred.bytes_to_store(), payload);
        assert_eq!(shred, Shred::new_from_serialized_shred(payload).unwrap());
        assert_eq!(
            layout::get_slot(packet.data(..).unwrap()),
            Some(shred.slot())
        );
        assert_eq!(
            get_shred_slot_index_type(&packet, &mut ShredFetchStats::default()),
            Some((shred.slot(), shred.index(), shred.shred_type()))
        );
    }

    #[test]
    fn test_shred_flags() {
        fn make_shred(is_last_data: bool, is_last_in_slot: bool, reference_tick: u8) -> Shred {
            let flags = if is_last_in_slot {
                assert!(is_last_data);
                ShredFlags::LAST_SHRED_IN_SLOT
            } else if is_last_data {
                ShredFlags::DATA_COMPLETE_SHRED
            } else {
                ShredFlags::empty()
            };
            Shred::new_from_data(
                0,   // slot
                0,   // index
                0,   // parent_offset
                &[], // data
                flags,
                reference_tick,
                0, // version
                0, // fec_set_index
            )
        }
        fn check_shred_flags(
            shred: &Shred,
            is_last_data: bool,
            is_last_in_slot: bool,
            reference_tick: u8,
        ) {
            assert_eq!(shred.data_complete(), is_last_data);
            assert_eq!(shred.last_in_slot(), is_last_in_slot);
            assert_eq!(shred.reference_tick(), reference_tick.min(63u8));
            assert_eq!(
                layout::get_reference_tick(shred.payload()).unwrap(),
                reference_tick.min(63u8),
            );
        }
        for is_last_data in [false, true] {
            for is_last_in_slot in [false, true] {
                // LAST_SHRED_IN_SLOT also implies DATA_COMPLETE_SHRED. So it
                // cannot be LAST_SHRED_IN_SLOT if not DATA_COMPLETE_SHRED.
                let is_last_in_slot = is_last_in_slot && is_last_data;
                for reference_tick in [0, 37, 63, 64, 80, 128, 255] {
                    let mut shred = make_shred(is_last_data, is_last_in_slot, reference_tick);
                    check_shred_flags(&shred, is_last_data, is_last_in_slot, reference_tick);
                    shred.set_last_in_slot();
                    check_shred_flags(&shred, true, true, reference_tick);
                }
            }
        }
    }

    #[test]
    fn test_shred_flags_serde() {
        let flags: ShredFlags = bincode::deserialize(&[0b0111_0001]).unwrap();
        assert!(flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(!flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
        assert_eq!((flags & ShredFlags::SHRED_TICK_REFERENCE_MASK).bits(), 49u8);
        assert_eq!(bincode::serialize(&flags).unwrap(), [0b0111_0001]);

        let flags: ShredFlags = bincode::deserialize(&[0b1110_0101]).unwrap();
        assert!(flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
        assert_eq!((flags & ShredFlags::SHRED_TICK_REFERENCE_MASK).bits(), 37u8);
        assert_eq!(bincode::serialize(&flags).unwrap(), [0b1110_0101]);

        let flags: ShredFlags = bincode::deserialize(&[0b1011_1101]).unwrap();
        assert!(!flags.contains(ShredFlags::DATA_COMPLETE_SHRED));
        assert!(!flags.contains(ShredFlags::LAST_SHRED_IN_SLOT));
        assert_eq!((flags & ShredFlags::SHRED_TICK_REFERENCE_MASK).bits(), 61u8);
        assert_eq!(bincode::serialize(&flags).unwrap(), [0b1011_1101]);
    }
}
