//! Interface and related utilities for interaction with the compiled WASM contracts.
//! Contracts have an isomorphic interface which partially maps to this interface,
//! allowing interaction between the runtime and the contracts themselves.
//!
//! This abstraction layer shouldn't leak beyond the contract handler.

use std::{
    borrow::{Borrow, Cow},
    io::{Cursor, Read},
    ops::{Deref, DerefMut},
    path::PathBuf,
};

use arrayvec::ArrayVec;
use blake2::{Blake2b512, Blake2s256, Digest};
use byteorder::LittleEndian;
use serde::{Deserialize, Deserializer, Serialize};

const CONTRACT_KEY_SIZE: usize = 64;

#[derive(Debug)]
pub enum ContractError {
    InvalidUpdate,
}

pub enum UpdateModification {
    ValidUpdate(State<'static>),
    NoChange,
}

pub trait ContractInterface {
    /// Verify that the state is valid, given the parameters.
    fn validate_state(parameters: Parameters<'static>, state: State<'static>) -> bool;

    /// Verify that a delta is valid - at least as much as possible.
    fn validate_delta(parameters: Parameters<'static>, delta: StateDelta<'static>) -> bool;

    /// Update the state to account for the state_delta, assuming it is valid.
    fn update_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        delta: StateDelta<'static>,
    ) -> Result<UpdateModification, ContractError>;

    /// Generate a concise summary of a state that can be used to create deltas
    /// relative to this state.
    fn summarize_state(
        parameters: Parameters<'static>,
        state: State<'static>,
    ) -> StateSummary<'static>;

    /// Generate a state delta using a summary from the current state.
    /// This along with [`Self::summarize_state`] allows flexible and efficient
    /// state synchronization between peers.
    fn get_state_delta(
        parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> StateDelta<'static>;

    /// Updates the current state from the provided summary.
    fn update_state_from_summary(
        parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<UpdateModification, ContractError>;
}

/// A complete contract specification requires a `parameters` section
/// and a `contract` section.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContractSpecification<'a> {
    parameters: Parameters<'a>,
    contract: ContractData<'a>,
    key: ContractKey,
}

impl ContractSpecification<'_> {
    pub fn new<'a>(
        contract: ContractData<'a>,
        parameters: Parameters<'a>,
    ) -> ContractSpecification<'a> {
        let key = ContractKey::from((&parameters, &contract));
        ContractSpecification {
            parameters,
            contract,
            key,
        }
    }

    pub fn key(&self) -> &ContractKey {
        &self.key
    }

    /// Data portion of the specification.
    pub fn data(&self) -> &ContractData {
        &self.contract
    }

    /// Parameters portion of the parameters.
    pub fn parameters(&self) -> &Parameters {
        &self.parameters
    }
}

impl TryFrom<Vec<u8>> for ContractSpecification<'static> {
    type Error = std::io::Error;

    fn try_from(data: Vec<u8>) -> Result<Self, Self::Error> {
        use byteorder::ReadBytesExt;
        let mut reader = Cursor::new(data);

        let params_len = reader.read_u64::<LittleEndian>()?;
        let mut params_buf = vec![0; params_len as usize];
        reader.read_exact(&mut params_buf)?;
        let parameters = Parameters::from(params_buf);

        let contract_len = reader.read_u64::<LittleEndian>()?;
        let mut contract_buf = vec![0; contract_len as usize];
        reader.read_exact(&mut contract_buf)?;
        let contract = ContractData::from(contract_buf);

        let key = ContractKey::from((&parameters, &contract));

        Ok(ContractSpecification {
            parameters,
            contract,
            key,
        })
    }
}

impl PartialEq for ContractSpecification<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for ContractSpecification<'_> {}

impl std::fmt::Display for ContractSpecification<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ContractSpec( key: ")?;
        internal_fmt_key(&self.key.spec, f)?;
        let data: String = if self.contract.data.len() > 8 {
            (&self.contract.data[..4])
                .iter()
                .map(|b| char::from(*b))
                .chain("...".chars())
                .chain((&self.contract.data[4..]).iter().map(|b| char::from(*b)))
                .collect()
        } else {
            self.contract.data.iter().copied().map(char::from).collect()
        };
        write!(f, ", data: [{}])", data)
    }
}

#[cfg(any(test, feature = "testing"))]
impl<'a> arbitrary::Arbitrary<'a> for ContractSpecification<'static> {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let contract: ContractData = u.arbitrary()?;
        let parameters: Vec<u8> = u.arbitrary()?;
        let parameters = Parameters::from(parameters);

        let key = ContractKey::from((&parameters, &contract));

        Ok(ContractSpecification {
            contract,
            parameters,
            key,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Parameters<'a>(Cow<'a, [u8]>);

impl<'a> Parameters<'a> {
    pub fn size(&self) -> usize {
        self.0.len()
    }
}

impl<'a> From<Vec<u8>> for Parameters<'a> {
    fn from(data: Vec<u8>) -> Self {
        Parameters(Cow::from(data))
    }
}

impl<'a> From<&'a [u8]> for Parameters<'a> {
    fn from(s: &'a [u8]) -> Self {
        Parameters(Cow::from(s))
    }
}

impl<'a> AsRef<[u8]> for Parameters<'a> {
    fn as_ref(&self) -> &[u8] {
        match &self.0 {
            Cow::Borrowed(arr) => arr,
            Cow::Owned(arr) => arr.as_ref(),
        }
    }
}

#[doc(hidden)]
#[repr(i32)]
pub enum UpdateResult {
    ValidUpdate = 0i32,
    ValidNoChange = 1i32,
    Invalid = 2i32,
}

impl From<ContractError> for UpdateResult {
    fn from(err: ContractError) -> Self {
        match err {
            ContractError::InvalidUpdate => UpdateResult::Invalid,
        }
    }
}

impl TryFrom<i32> for UpdateResult {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::ValidUpdate),
            1 => Ok(Self::ValidNoChange),
            2 => Ok(Self::Invalid),
            _ => Err(()),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "testing", derive(arbitrary::Arbitrary))]
pub struct State<'a>(Cow<'a, [u8]>);

impl<'a> State<'a> {
    pub fn size(&self) -> usize {
        self.0.len()
    }

    pub fn into_owned(self) -> Vec<u8> {
        self.0.into_owned()
    }

    pub fn to_mut(&mut self) -> &mut Vec<u8> {
        self.0.to_mut()
    }
}

impl<'a> From<Vec<u8>> for State<'a> {
    fn from(state: Vec<u8>) -> Self {
        State(Cow::from(state))
    }
}

impl<'a> From<&'a [u8]> for State<'a> {
    fn from(state: &'a [u8]) -> Self {
        State(Cow::from(state))
    }
}

impl<'a> AsRef<[u8]> for State<'a> {
    fn as_ref(&self) -> &[u8] {
        match &self.0 {
            Cow::Borrowed(arr) => arr,
            Cow::Owned(arr) => arr.as_ref(),
        }
    }
}

impl<'a> Deref for State<'a> {
    type Target = Cow<'a, [u8]>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> DerefMut for State<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct StateDelta<'a>(Cow<'a, [u8]>);

impl<'a> StateDelta<'a> {
    pub fn size(&self) -> usize {
        self.0.len()
    }

    pub fn into_owned(self) -> Vec<u8> {
        self.0.into_owned()
    }
}

impl<'a> From<Vec<u8>> for StateDelta<'a> {
    fn from(delta: Vec<u8>) -> Self {
        StateDelta(Cow::from(delta))
    }
}

impl<'a> From<&'a [u8]> for StateDelta<'a> {
    fn from(delta: &'a [u8]) -> Self {
        StateDelta(Cow::from(delta))
    }
}

impl<'a> AsRef<[u8]> for StateDelta<'a> {
    fn as_ref(&self) -> &[u8] {
        match &self.0 {
            Cow::Borrowed(arr) => arr,
            Cow::Owned(arr) => arr.as_ref(),
        }
    }
}

impl<'a> Deref for StateDelta<'a> {
    type Target = Cow<'a, [u8]>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> DerefMut for StateDelta<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub struct StateSummary<'a>(Cow<'a, [u8]>);

impl<'a> StateSummary<'a> {
    pub fn into_owned(self) -> Vec<u8> {
        self.0.into_owned()
    }
}

impl<'a> From<Vec<u8>> for StateSummary<'a> {
    fn from(state: Vec<u8>) -> Self {
        StateSummary(Cow::from(state))
    }
}

impl<'a> From<&'a [u8]> for StateSummary<'a> {
    fn from(state: &'a [u8]) -> Self {
        StateSummary(Cow::from(state))
    }
}

impl<'a> StateSummary<'a> {
    pub fn size(&self) -> usize {
        self.0.len()
    }
}

impl<'a> AsRef<[u8]> for StateSummary<'a> {
    fn as_ref(&self) -> &[u8] {
        match &self.0 {
            Cow::Borrowed(arr) => arr,
            Cow::Owned(arr) => arr.as_ref(),
        }
    }
}

impl<'a> Deref for StateSummary<'a> {
    type Target = Cow<'a, [u8]>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> DerefMut for StateSummary<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// The executable contract.
///
/// It is the part of the executable belonging to the full specification
/// and does not include any other metadata (like the parameters).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContractData<'a> {
    data: Cow<'a, [u8]>,
    #[serde(serialize_with = "<[_]>::serialize")]
    #[serde(deserialize_with = "contract_key_deser")]
    key: [u8; CONTRACT_KEY_SIZE],
}

impl ContractData<'_> {
    pub fn key(&self) -> &[u8; CONTRACT_KEY_SIZE] {
        &self.key
    }

    pub fn data(&self) -> &[u8] {
        &*self.data
    }

    pub fn into_data(self) -> Vec<u8> {
        self.data.to_owned().to_vec()
    }

    fn gen_key(data: &[u8]) -> [u8; CONTRACT_KEY_SIZE] {
        let mut hasher = Blake2s256::new();
        hasher.update(&data);
        let key_arr = hasher.finalize();
        debug_assert_eq!((&key_arr[..]).len(), CONTRACT_KEY_SIZE);
        let mut key = [0; CONTRACT_KEY_SIZE];
        key.copy_from_slice(&key_arr);
        key
    }
}

impl From<Vec<u8>> for ContractData<'static> {
    fn from(data: Vec<u8>) -> Self {
        let key = ContractData::gen_key(&data);
        ContractData {
            data: Cow::from(data),
            key,
        }
    }
}

impl<'a> From<&'a [u8]> for ContractData<'a> {
    fn from(data: &'a [u8]) -> ContractData {
        let key = ContractData::gen_key(data);
        ContractData {
            data: Cow::from(data),
            key,
        }
    }
}

impl PartialEq for ContractData<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for ContractData<'_> {}

#[cfg(any(test, feature = "testing"))]
impl<'a> arbitrary::Arbitrary<'a> for ContractData<'static> {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let data: Vec<u8> = u.arbitrary()?;
        Ok(ContractData::from(data))
    }
}

impl std::fmt::Display for ContractData<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Contract( key: ")?;
        internal_fmt_key(&self.key, f)?;
        let data: String = if self.data.len() > 8 {
            (&self.data[..4])
                .iter()
                .map(|b| char::from(*b))
                .chain("...".chars())
                .chain((&self.data[4..]).iter().map(|b| char::from(*b)))
                .collect()
        } else {
            self.data.iter().copied().map(char::from).collect()
        };
        write!(f, ", data: [{}])", data)
    }
}

/// The key representing a contract.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize, Hash)]
#[cfg_attr(any(test, feature = "testing"), derive(arbitrary::Arbitrary))]
pub struct ContractKey {
    #[serde(deserialize_with = "contract_key_deser")]
    #[serde(serialize_with = "<[_]>::serialize")]
    spec: [u8; CONTRACT_KEY_SIZE],
    #[serde(deserialize_with = "contract_key_deser")]
    #[serde(serialize_with = "<[_]>::serialize")]
    contract: [u8; CONTRACT_KEY_SIZE],
}

impl<'a, T, U> From<(T, U)> for ContractKey
where
    T: Borrow<Parameters<'a>>,
    U: Borrow<ContractData<'a>>,
{
    fn from(spec: (T, U)) -> Self {
        let (parameters, contract) = (spec.0.borrow(), spec.1.borrow());

        let contract_hash = contract.key();

        let mut hasher = Blake2b512::new();
        hasher.update(contract_hash);
        hasher.update(parameters.as_ref());
        let full_key_arr = hasher.finalize();

        debug_assert_eq!((&full_key_arr[..]).len(), CONTRACT_KEY_SIZE);
        let mut spec = [0; CONTRACT_KEY_SIZE];
        spec.copy_from_slice(&full_key_arr);
        Self {
            spec,
            contract: *contract_hash,
        }
    }
}

impl ContractKey {
    /// Gets the whole spec key hash.
    pub fn bytes(&self) -> &[u8] {
        self.spec.as_ref()
    }

    /// Returns the hash of the contract data only.
    pub fn contract_part(&self) -> &[u8; CONTRACT_KEY_SIZE] {
        &self.contract
    }

    pub fn hex_decode(
        encoded_contract: impl Into<String>,
        parameters: Parameters,
    ) -> Result<Self, hex::FromHexError> {
        let mut contract = [0; 64];
        hex::decode_to_slice(encoded_contract.into(), &mut contract)?;

        let mut hasher = Blake2b512::new();
        hasher.update(&contract);
        hasher.update(parameters.as_ref());
        let full_key_arr = hasher.finalize();

        let mut spec = [0; CONTRACT_KEY_SIZE];
        spec.copy_from_slice(&full_key_arr);
        Ok(Self { spec, contract })
    }

    pub fn hex_encode(&self) -> String {
        hex::encode(self.spec)
    }
}

impl From<ContractKey> for PathBuf {
    fn from(val: ContractKey) -> Self {
        let r = hex::encode(val.spec);
        PathBuf::from(r)
    }
}

impl Deref for ContractKey {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.spec
    }
}

impl std::fmt::Display for ContractKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ContractKey(")?;
        internal_fmt_key(&self.spec, f)?;
        write!(f, ")")
    }
}

fn internal_fmt_key(
    key: &[u8; CONTRACT_KEY_SIZE],
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    let r = hex::encode(key);
    write!(f, "{}", &r[..8])
}

// A bit wasteful but cannot deserialize directly into [u8; 64]
// with current version of serde
fn contract_key_deser<'de, D>(deserializer: D) -> Result<[u8; 64], D::Error>
where
    D: Deserializer<'de>,
{
    let data: ArrayVec<u8, 64> = Deserialize::deserialize(deserializer)?;
    data.into_inner()
        .map_err(|_| <D::Error as serde::de::Error>::custom("invalid key length"))
}

#[cfg(test)]
mod test {
    use super::*;
    use once_cell::sync::Lazy;
    use rand::{rngs::SmallRng, Rng, SeedableRng};

    static RND_BYTES: Lazy<[u8; 1024]> = Lazy::new(|| {
        let mut bytes = [0; 1024];
        let mut rng = SmallRng::from_entropy();
        rng.fill(&mut bytes);
        bytes
    });

    #[test]
    fn key_ser() -> Result<(), Box<dyn std::error::Error>> {
        let mut gen = arbitrary::Unstructured::new(&*RND_BYTES);
        let expected: ContractKey = gen.arbitrary()?;
        let encoded = hex::encode(expected.bytes());
        // eprintln!("encoded key: {encoded}");

        let serialized = bincode::serialize(&expected)?;
        let deserialized: ContractKey = bincode::deserialize(&serialized)?;
        let decoded = hex::encode(deserialized.bytes());
        assert_eq!(encoded, decoded);
        assert_eq!(deserialized, expected);
        Ok(())
    }

    #[test]
    fn contract_ser() -> Result<(), Box<dyn std::error::Error>> {
        let mut gen = arbitrary::Unstructured::new(&*RND_BYTES);
        let expected: ContractSpecification = gen.arbitrary()?;

        let serialized = bincode::serialize(&expected)?;
        let deserialized: ContractSpecification = bincode::deserialize(&serialized)?;
        assert_eq!(deserialized, expected);
        Ok(())
    }
}
