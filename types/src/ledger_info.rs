// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    account_address::AccountAddress,
    transaction::Version,
    validator_verifier::{ValidatorVerifier, VerifyError},
};
use canonical_serialization::{CanonicalSerialize, CanonicalSerializer, SimpleSerializer};
use crypto::{
    hash::{CryptoHash, CryptoHasher, LedgerInfoHasher},
    HashValue, *,
};
use failure::prelude::*;
#[cfg(any(test, feature = "testing"))]
use proptest_derive::Arbitrary;
use proto_conv::{FromProto, IntoProto};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
};

/// This structure serves a dual purpose.
///
/// First, if this structure is signed by 2f+1 validators it signifies the state of the ledger at
/// version `version` -- it contains the transaction accumulator at that version which commits to
/// all historical transactions. This structure may be expanded to include other information that
/// is derived from that accumulator (e.g. the current time according to the time contract) to
/// reduce the number of proofs a client must get.
///
/// Second, the structure contains a `consensus_data_hash` value. This is the hash of an internal
/// data structure that represents a block that is voted on in HotStuff. If 2f+1 signatures are
/// gathered on the same ledger info that represents a Quorum Certificate (QC) on the HotStuff
/// data.
///
/// Combining these two concepts when the consensus algorithm votes on a block B it votes for a
/// LedgerInfo with the `version` being the latest version that will be committed if B gets 2f+1
/// votes. It sets `consensus_data_hash` to represent B so that if those 2f+1 votes are gathered a
/// QC is formed on B.
#[derive(Clone, Debug, Eq, PartialEq, IntoProto, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "testing"), derive(Arbitrary))]
#[ProtoType(crate::proto::ledger_info::LedgerInfo)]
pub struct LedgerInfo {
    /// The version of latest transaction in the ledger.
    version: Version,

    /// The root hash of transaction accumulator.
    transaction_accumulator_hash: HashValue,

    /// Hash of consensus specific data that is opaque to all parts of the system other than
    /// consensus.
    consensus_data_hash: HashValue,

    /// Block id of the last committed block corresponding to this LedgerInfo
    /// as reported by consensus.
    consensus_block_id: HashValue,

    /// Epoch number corresponds to the set of validators that are active for this ledger info.
    epoch_num: u64,

    // Timestamp that represents the microseconds since the epoch (unix time) that is
    // generated by the proposer of the block.  This is strictly increasing with every block.
    // If a client reads a timestamp > the one they specified for transaction expiration time,
    // they can be certain that their transaction will never be included in a block in the future
    // (assuming that their transaction has not yet been included)
    timestamp_usecs: u64,
}

impl Display for LedgerInfo {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(
            f,
            "LedgerInfo: [committed_block_id: {}, version: {}, epoch_num: {}, timestamp (us): {}]",
            self.consensus_block_id(),
            self.version(),
            self.epoch_num(),
            self.timestamp_usecs()
        )
    }
}

impl LedgerInfo {
    /// Constructs a `LedgerInfo` object at a specific version using given transaction accumulator
    /// root and hot stuff data hash.
    pub fn new(
        version: Version,
        transaction_accumulator_hash: HashValue,
        consensus_data_hash: HashValue,
        consensus_block_id: HashValue,
        epoch_num: u64,
        timestamp_usecs: u64,
    ) -> Self {
        LedgerInfo {
            version,
            transaction_accumulator_hash,
            consensus_data_hash,
            consensus_block_id,
            epoch_num,
            timestamp_usecs,
        }
    }

    /// Returns the version of this `LedgerInfo`.
    pub fn version(&self) -> Version {
        self.version
    }

    /// Returns the transaction accumulator root of this `LedgerInfo`.
    pub fn transaction_accumulator_hash(&self) -> HashValue {
        self.transaction_accumulator_hash
    }

    /// Returns hash of consensus data in this `LedgerInfo`.
    pub fn consensus_data_hash(&self) -> HashValue {
        self.consensus_data_hash
    }

    pub fn consensus_block_id(&self) -> HashValue {
        self.consensus_block_id
    }

    pub fn set_consensus_data_hash(&mut self, consensus_data_hash: HashValue) {
        self.consensus_data_hash = consensus_data_hash;
    }

    pub fn epoch_num(&self) -> u64 {
        self.epoch_num
    }

    pub fn timestamp_usecs(&self) -> u64 {
        self.timestamp_usecs
    }

    /// A ledger info is nominal if it's not certifying any real version.
    pub fn is_zero(&self) -> bool {
        self.version == 0
    }
}

impl FromProto for LedgerInfo {
    type ProtoType = crate::proto::ledger_info::LedgerInfo;

    fn from_proto(proto: Self::ProtoType) -> Result<Self> {
        Ok(LedgerInfo::new(
            proto.get_version(),
            HashValue::from_slice(proto.get_transaction_accumulator_hash())?,
            HashValue::from_slice(proto.get_consensus_data_hash())?,
            HashValue::from_slice(proto.get_consensus_block_id())?,
            proto.get_epoch_num(),
            proto.get_timestamp_usecs(),
        ))
    }
}

impl CanonicalSerialize for LedgerInfo {
    fn serialize(&self, serializer: &mut impl CanonicalSerializer) -> Result<()> {
        serializer
            .encode_u64(self.version)?
            .encode_bytes(self.transaction_accumulator_hash.as_ref())?
            .encode_bytes(self.consensus_data_hash.as_ref())?
            .encode_bytes(self.consensus_block_id.as_ref())?
            .encode_u64(self.epoch_num)?
            .encode_u64(self.timestamp_usecs)?;
        Ok(())
    }
}

impl CryptoHash for LedgerInfo {
    type Hasher = LedgerInfoHasher;

    fn hash(&self) -> HashValue {
        let mut state = Self::Hasher::default();
        state.write(
            &SimpleSerializer::<Vec<u8>>::serialize(self).expect("Serialization should work."),
        );
        state.finish()
    }
}

// The validator node returns this structure which includes signatures
// from each validator to confirm the state.  The client needs to only pass back
// the LedgerInfo element since the validator node doesn't need to know the signatures
// again when the client performs a query, those are only there for the client
// to be able to verify the state
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LedgerInfoWithSignatures<Sig> {
    ledger_info: LedgerInfo,
    /// The validator is identified by its account address: in order to verify a signature
    /// one needs to retrieve the public key of the validator for the given epoch.
    signatures: HashMap<AccountAddress, Sig>,
}

impl<Sig> Display for LedgerInfoWithSignatures<Sig> {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{}", self.ledger_info)
    }
}

impl<Sig: Signature> LedgerInfoWithSignatures<Sig> {
    pub fn new(ledger_info: LedgerInfo, signatures: HashMap<AccountAddress, Sig>) -> Self {
        LedgerInfoWithSignatures {
            ledger_info,
            signatures,
        }
    }

    pub fn ledger_info(&self) -> &LedgerInfo {
        &self.ledger_info
    }

    pub fn add_signature(&mut self, validator: AccountAddress, signature: Sig) {
        self.signatures.entry(validator).or_insert(signature);
    }

    pub fn signatures(&self) -> &HashMap<AccountAddress, Sig> {
        &self.signatures
    }

    pub fn verify(
        &self,
        validator: &ValidatorVerifier<Sig::VerifyingKeyMaterial>,
    ) -> ::std::result::Result<(), VerifyError> {
        if self.ledger_info.is_zero() {
            // We're not trying to verify nominal ledger info that does not carry any information.
            return Ok(());
        }
        let ledger_hash = self.ledger_info().hash();
        validator.batch_verify_aggregated_signature(ledger_hash, self.signatures())
    }
}

impl<Sig: Signature> FromProto for LedgerInfoWithSignatures<Sig> {
    type ProtoType = crate::proto::ledger_info::LedgerInfoWithSignatures;

    fn from_proto(mut proto: Self::ProtoType) -> Result<Self> {
        let ledger_info = LedgerInfo::from_proto(proto.take_ledger_info())?;

        let signatures_proto = proto.take_signatures();
        let num_signatures = signatures_proto.len();
        let signatures = signatures_proto
            .into_iter()
            .map(|proto| {
                let validator_id = AccountAddress::from_proto(proto.get_validator_id().to_vec())?;
                let signature_bytes: &[u8] = proto.get_signature();
                let signature = Sig::try_from(signature_bytes)?;
                Ok((validator_id, signature))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        ensure!(
            signatures.len() == num_signatures,
            "Signatures should be from different validators."
        );

        Ok(LedgerInfoWithSignatures {
            ledger_info,
            signatures,
        })
    }
}

impl<Sig: Signature> IntoProto for LedgerInfoWithSignatures<Sig> {
    type ProtoType = crate::proto::ledger_info::LedgerInfoWithSignatures;

    fn into_proto(self) -> Self::ProtoType {
        let mut proto = Self::ProtoType::new();
        proto.set_ledger_info(self.ledger_info.into_proto());
        self.signatures
            .into_iter()
            .for_each(|(validator_id, signature)| {
                let mut validator_signature = crate::proto::ledger_info::ValidatorSignature::new();
                validator_signature.set_validator_id(validator_id.into_proto());
                validator_signature.set_signature(signature.to_bytes().to_vec());
                proto.mut_signatures().push(validator_signature)
            });
        proto
    }
}
