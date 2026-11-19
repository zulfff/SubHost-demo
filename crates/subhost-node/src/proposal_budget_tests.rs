//! Size-only fixtures: byte fillers model large encodings, never valid evidence.
//!
//! This module is linked below consensus_journal so the recovery assertion uses
//! its real private JournalData shape rather than a second envelope definition.

use super::{JournalData, MAGIC};
use crate::{consensus_block_budget, NodeError, MAX_CONSENSUS_CHECKPOINT_BYTES};
use subhost_consensus::{
    driver::{
        BlockProposal, DriverCheckpoint, PrepareCertificate, PreparedBlock, TimeoutCertificate,
        ViewChange, Vote, VotePhase,
    },
    QuorumCertificate,
};
use subhost_core::{
    encode_canonical, Address, Block, BlockHeader, Hash, Transaction, TransactionSignature,
    TransactionType, ValidatorSignature, CALLDATA_HEADER_VERSION,
};
use subhost_network::{NetworkMessage, MAX_WIRE_BYTES};

fn signer(index: usize) -> Address {
    let mut bytes = [u8::MAX; 20];
    bytes[0] = u8::try_from(index).unwrap();
    Address::new(bytes)
}

fn signatures(count: usize) -> Vec<(Address, Vec<u8>)> {
    (0..count).map(|index| (signer(index), vec![u8::MAX; 96])).collect()
}

fn full_block(budget: usize) -> Block {
    let digest = Hash::new([u8::MAX; 32]);
    let mut block = Block {
        header: BlockHeader {
            // Widest VALID version: the accept-only codec (Q-037 L2) refuses to
            // encode versions above CALLDATA_HEADER_VERSION, so this size-only
            // sentinel must use the highest encodable version, not u32::MAX.
            // bincode encodes the version as a fixed-width u32, so the value
            // does not change framing size; every other field keeps its max.
            version: CALLDATA_HEADER_VERSION,
            chain_id: u64::MAX,
            height: u64::MAX - 1,
            timestamp: u64::MAX,
            parent_hash: digest,
            state_root: digest,
            tx_root: digest,
            receipt_root: digest,
            validator: signer(0),
            gas_used: u64::MAX,
            gas_limit: u64::MAX,
            extra_data: Vec::new(),
            base_fee_per_gas: 0,
        },
        transactions: vec![Transaction {
            tx_type: TransactionType::Transfer,
            nonce: u64::MAX,
            from: signer(0),
            to: Some(signer(1)),
            value: u128::MAX,
            gas_price: u128::MAX,
            gas_limit: u64::MAX,
            data: Vec::new(),
            chain_id: u64::MAX,
            public_key: [u8::MAX; 32],
            signature: TransactionSignature { r: [u8::MAX; 32], s: [u8::MAX; 32], v: u8::MAX },
        }],
        signatures: vec![ValidatorSignature { validator: signer(0), signature: vec![u8::MAX; 96] }],
    };
    // Measure the fixed framing using the production codec, then fill its body
    // right up to the public budget. No budget formula is repeated in this test.
    let framing = encode_canonical(&block).len();
    block.transactions[0].data.resize(budget.checked_sub(framing).unwrap(), u8::MAX);
    assert_eq!(encode_canonical(&block).len(), budget);
    block
}

fn timeout(block: &Block, count: usize) -> TimeoutCertificate {
    let prepared = PreparedBlock {
        block: block.clone(),
        qc: PrepareCertificate {
            protocol_context: Hash::new([u8::MAX; 32]),
            view: u64::MAX - 2,
            block_hash: block.hash(),
            signatures: signatures(count),
        },
    };
    TimeoutCertificate {
        view: u64::MAX - 1,
        height: block.header.height,
        tip: block.header.parent_hash,
        messages: (0..count)
            .map(|index| ViewChange {
                protocol_context: prepared.qc.protocol_context,
                view: u64::MAX - 1,
                height: block.header.height,
                tip: block.header.parent_hash,
                validator: signer(index),
                prepared: Some(prepared.clone()),
                signature: vec![u8::MAX; 96],
            })
            .collect(),
    }
}

fn proposal(block: &Block, certificate: &TimeoutCertificate) -> BlockProposal {
    BlockProposal {
        protocol_context: Hash::new([u8::MAX; 32]),
        view: certificate.view,
        block: block.clone(),
        proposer: signer(0),
        signature: vec![u8::MAX; 96],
        timeout_certificate: Some(certificate.clone()),
    }
}

fn final_qc(block: &Block, count: usize) -> QuorumCertificate {
    QuorumCertificate {
        protocol_context: Hash::new([u8::MAX; 32]),
        view: u64::MAX - 1,
        block_hash: block.hash(),
        signatures: signatures(count),
    }
}

fn assert_wire_fit(message: NetworkMessage, count: usize) {
    let wire = serde_json::to_vec(&message).unwrap();
    assert!(wire.len() <= MAX_WIRE_BYTES, "{count} validators need {} wire bytes", wire.len());
    assert!(
        message.validate().is_ok(),
        "actual serialized envelope must pass transport validation"
    );
}

#[test]
fn rotated_proposals_with_full_prepared_reports_fit_the_actual_wire_envelope() {
    for count in [1, 4, 16, 64] {
        let budget = consensus_block_budget(count).unwrap();
        let block = full_block(budget);
        let tc = timeout(&block, count);
        assert_eq!(tc.messages.len(), count);
        assert!(tc.messages.iter().all(|report| report
            .prepared
            .as_ref()
            .unwrap()
            .qc
            .signatures
            .len()
            == count));
        let tc_data = encode_canonical(&tc);
        assert!(tc_data.len() <= MAX_WIRE_BYTES);
        assert_wire_fit(NetworkMessage::TimeoutCertificate { data: tc_data }, count);
        let proposal_data = encode_canonical(&proposal(&block, &tc));
        assert!(proposal_data.len() <= MAX_WIRE_BYTES);
        assert_wire_fit(NetworkMessage::ConsensusProposal { data: proposal_data }, count);
    }
}

#[test]
fn full_key_free_checkpoint_and_prepared_commit_fit_the_real_journal_envelope() {
    for count in [1, 4, 16, 64] {
        let block = full_block(consensus_block_budget(count).unwrap());
        let tc = timeout(&block, count);
        let prepared = tc.messages[0].prepared.as_ref().unwrap().clone();
        let qc = final_qc(&block, count);
        let votes = [VotePhase::Prepare, VotePhase::Commit]
            .into_iter()
            .flat_map(|phase| {
                (0..count).map(move |index| Vote {
                    view: u64::MAX - 1,
                    block_hash: Hash::new([u8::MAX; 32]),
                    validator: signer(index),
                    signature: vec![u8::MAX; 96],
                    phase,
                })
            })
            .collect();
        let data = JournalData {
            version: 2,
            chain_id: u64::MAX,
            genesis: Hash::new([u8::MAX; 32]),
            identity: Some(signer(0)),
            checkpoint: DriverCheckpoint {
                height: block.header.height - 1,
                tip: block.header.parent_hash,
                high_qc: Some(qc.clone()),
                proposal: Some(proposal(&block, &tc)),
                votes,
                view: tc.view,
                prepared: Some(prepared),
                timeout_certificate: Some(tc.clone()),
                view_changes: tc.messages.clone(),
                own_view_change: Some(tc.messages[0].clone()),
            },
            prepared_commit: Some((block, qc)),
        };
        let payload = encode_canonical(&data);
        let checksum = Hash::from_data(&payload);
        let mut envelope = MAGIC.to_vec();
        envelope.extend_from_slice(&payload);
        envelope.extend_from_slice(checksum.as_bytes());
        assert!(
            envelope.len() <= MAX_CONSENSUS_CHECKPOINT_BYTES,
            "{count} validators need {} checkpoint bytes",
            envelope.len()
        );
    }
}

#[test]
fn unsupported_validator_counts_fail_before_any_budget_arithmetic() {
    for count in [0, 65, usize::MAX] {
        assert!(matches!(
            consensus_block_budget(count),
            Err(NodeError::InvalidConsensusConfiguration(_))
        ));
    }
}
