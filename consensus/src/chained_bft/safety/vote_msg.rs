// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    chained_bft::common::{Author, Round},
    state_replication::ExecutedState,
};
use canonical_serialization::{CanonicalSerialize, CanonicalSerializer, SimpleSerializer};
use crypto::{
    hash::{CryptoHash, CryptoHasher, VoteMsgHasher},
    HashValue, Signature,
};
use failure::Result as ProtoResult;
use network::proto::Vote as ProtoVote;
use nextgen_crypto::ed25519::*;
use proto_conv::{FromProto, IntoProto};
use serde::{Deserialize, Serialize};
use std::{
    convert::TryFrom,
    fmt::{Display, Formatter},
};
use types::{
    ledger_info::LedgerInfo,
    validator_signer::ValidatorSigner,
    validator_verifier::{ValidatorVerifier, VerifyError},
};

/// VoteMsg verification errors.
#[derive(Debug, Fail, PartialEq)]
pub enum VoteMsgVerificationError {
    /// The internal consensus data of LedgerInfo doesn't match the vote info.
    #[fail(display = "ConsensusDataMismatch")]
    ConsensusDataMismatch,
    /// The signature doesn't pass verification
    #[fail(display = "SigVerifyError: {}", _0)]
    SigVerifyError(VerifyError),
}

// Internal use only. Contains all the fields in VoteMsgSerializer that contributes to the
// computation of its hash.
struct VoteMsgSerializer {
    proposed_block_id: HashValue,
    executed_state: ExecutedState,
    round: Round,
}

impl CanonicalSerialize for VoteMsgSerializer {
    fn serialize(&self, serializer: &mut impl CanonicalSerializer) -> failure::Result<()> {
        serializer.encode_raw_bytes(self.proposed_block_id.as_ref())?;
        serializer.encode_struct(&self.executed_state)?;
        serializer.encode_u64(self.round)?;
        Ok(())
    }
}

impl CryptoHash for VoteMsgSerializer {
    type Hasher = VoteMsgHasher;

    fn hash(&self) -> HashValue {
        let mut state = Self::Hasher::default();
        state.write(
            SimpleSerializer::<Vec<u8>>::serialize(self)
                .expect("Should serialize.")
                .as_ref(),
        );
        state.finish()
    }
}

/// VoteMsg is the struct that is ultimately sent by the voter in response for
/// receiving a proposal.
/// VoteMsg carries the `LedgerInfo` of a block that is going to be committed in case this vote
/// is gathers QuorumCertificate (see the detailed explanation in the comments of `LedgerInfo`).
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct VoteMsg {
    /// The id of the proposed block.
    proposed_block_id: HashValue,
    /// The id of the state generated by the StateExecutor after executing the proposed block.
    executed_state: ExecutedState,
    /// The round of the block.
    round: Round,
    /// The identity of the voter.
    author: Author,
    /// LedgerInfo of a block that is going to be committed in case this vote gathers QC.
    ledger_info: LedgerInfo,
    /// Signature of the LedgerInfo
    signature: Signature,
}

impl Display for VoteMsg {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(
            f,
            "Vote: [block id: {}, round: {:02}, author: {}, {}]",
            self.proposed_block_id,
            self.round,
            self.author.short_str(),
            self.ledger_info
        )
    }
}

impl VoteMsg {
    pub fn new(
        proposed_block_id: HashValue,
        executed_state: ExecutedState,
        round: Round,
        author: Author,
        mut ledger_info_placeholder: LedgerInfo,
        validator_signer: &ValidatorSigner<Ed25519PrivateKey>,
    ) -> Self {
        ledger_info_placeholder.set_consensus_data_hash(Self::vote_digest(
            proposed_block_id,
            executed_state,
            round,
        ));
        let li_sig = validator_signer
            .sign_message(ledger_info_placeholder.hash())
            .expect("Failed to sign LedgerInfo");
        Self {
            proposed_block_id,
            executed_state,
            round,
            author,
            ledger_info: ledger_info_placeholder,
            signature: li_sig.into(),
        }
    }

    /// Return the proposed block id
    pub fn proposed_block_id(&self) -> HashValue {
        self.proposed_block_id
    }

    /// Return the executed state of the proposed block
    pub fn executed_state(&self) -> ExecutedState {
        self.executed_state
    }

    /// Return the round of the block
    pub fn round(&self) -> Round {
        self.round
    }

    /// Return the author of the vote
    pub fn author(&self) -> Author {
        self.author
    }

    /// Return the LedgerInfo associated with this vote
    pub fn ledger_info(&self) -> &LedgerInfo {
        &self.ledger_info
    }

    /// Return the signature of the vote
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// Verifies that the consensus data hash of LedgerInfo corresponds to the vote info,
    /// and then verifies the signature.
    pub fn verify(
        &self,
        validator: &ValidatorVerifier<Ed25519PublicKey>,
    ) -> Result<(), VoteMsgVerificationError> {
        if self.ledger_info.consensus_data_hash() != self.vote_hash() {
            return Err(VoteMsgVerificationError::ConsensusDataMismatch);
        }
        validator
            .verify_signature(
                self.author(),
                self.ledger_info.hash(),
                &(self.signature().clone().into()),
            )
            .map_err(VoteMsgVerificationError::SigVerifyError)
    }

    /// Return the hash of this struct
    pub fn vote_hash(&self) -> HashValue {
        Self::vote_digest(self.proposed_block_id, self.executed_state, self.round)
    }

    /// Return a digest of the vote
    pub fn vote_digest(
        proposed_block_id: HashValue,
        executed_state: ExecutedState,
        round: Round,
    ) -> HashValue {
        VoteMsgSerializer {
            proposed_block_id,
            executed_state,
            round,
        }
        .hash()
    }
}

impl IntoProto for VoteMsg {
    type ProtoType = ProtoVote;

    fn into_proto(self) -> Self::ProtoType {
        let mut proto = Self::ProtoType::new();
        proto.set_proposed_block_id(self.proposed_block_id().into());
        proto.set_executed_state_id(self.executed_state().state_id.into());
        proto.set_version(self.executed_state().version);
        proto.set_round(self.round);
        proto.set_author(self.author.into());
        proto.set_ledger_info(self.ledger_info.into_proto());
        proto.set_signature(self.signature.to_compact().as_ref().into());
        proto
    }
}

impl FromProto for VoteMsg {
    type ProtoType = ProtoVote;

    fn from_proto(mut object: Self::ProtoType) -> ProtoResult<Self> {
        let proposed_block_id = HashValue::from_slice(object.get_proposed_block_id())?;
        let state_id = HashValue::from_slice(object.get_executed_state_id())?;
        let version = object.get_version();
        let round = object.get_round();
        let author = Author::try_from(object.take_author())?;
        let ledger_info = LedgerInfo::from_proto(object.take_ledger_info())?;
        let signature = Signature::from_compact(object.get_signature())?;
        Ok(VoteMsg {
            proposed_block_id,
            executed_state: ExecutedState { state_id, version },
            round,
            author,
            ledger_info,
            signature,
        })
    }
}
