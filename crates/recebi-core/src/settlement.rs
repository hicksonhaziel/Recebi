use std::{collections::HashSet, hash::BuildHasher};

use sha2::{Digest, Sha256};
use spl_token_interface::instruction::TokenInstruction;

use crate::{
    AtomicAmount, GenesisHash, PublicKey, ReceivableId, Reference, solana::derive_classic_ata,
};

const CLASSIC_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const MAX_ACCOUNT_KEYS: usize = 64;
const MAX_INSTRUCTIONS: usize = 32;
const MAX_ACCOUNTS_PER_INSTRUCTION: usize = 16;
const MAX_INSTRUCTION_DATA_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountMeta {
    pub key: PublicKey,
    pub is_signer: bool,
    pub is_writable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledInstruction {
    pub program_id_index: u8,
    pub account_indices: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenAccountSnapshot {
    pub address: PublicKey,
    pub mint: PublicKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionSnapshot {
    pub signature: String,
    pub slot: u64,
    pub block_time_unix: Option<i64>,
    pub finalized: bool,
    pub succeeded: bool,
    pub cluster_genesis_hash: GenesisHash,
    pub address_tables_resolved: bool,
    pub account_keys: Vec<AccountMeta>,
    pub instructions: Vec<CompiledInstruction>,
    pub token_accounts: Vec<TokenAccountSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlementExpectation {
    pub receivable_id: ReceivableId,
    pub cluster_genesis_hash: GenesisHash,
    pub merchant_wallet: PublicKey,
    pub mint: PublicKey,
    pub amount: AtomicAmount,
    pub token_decimals: u8,
    pub reference: Reference,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlementEvidence {
    pub signature: String,
    pub slot: u64,
    pub block_time_unix: Option<i64>,
    pub cluster_genesis_hash: GenesisHash,
    pub recipient: PublicKey,
    pub mint: PublicKey,
    pub amount: AtomicAmount,
    pub transfer_instruction_position: usize,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnderpaymentEvidence {
    pub signature: String,
    pub slot: u64,
    pub block_time_unix: Option<i64>,
    pub cluster_genesis_hash: GenesisHash,
    pub recipient: PublicKey,
    pub mint: PublicKey,
    pub expected_amount: AtomicAmount,
    pub received_amount: AtomicAmount,
    pub shortfall_amount: AtomicAmount,
    pub transfer_instruction_position: usize,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettlementAssessment {
    Exact(SettlementEvidence),
    Underpayment(UnderpaymentEvidence),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettlementVerdict {
    NotFinalized,
    TransactionFailed,
    BoundsExceeded,
    InvalidSignature,
    UnsupportedToken2022,
    UnsupportedProgram,
    MalformedInstruction,
    MissingTokenAccount,
    WrongMint,
    WrongCluster,
    WrongDecimals,
    WrongRecipient,
    SelfTransfer,
    WrongAmount,
    MissingReference,
    UnsafeReference,
    MultipleCandidateTransfers,
    NoExactTransfer,
    UnresolvedAddressTable,
    DuplicateSignature,
    ReferenceReused,
}

/// Verifies one exact classic-SPL settlement from an immutable, already bounded
/// snapshot. It has no RPC, database, wallet, or LLM dependency.
///
/// # Errors
///
/// Returns one explicit fail-closed verdict for every rejected snapshot shape.
///
pub fn verify_settlement(
    snapshot: &TransactionSnapshot,
    expected: &SettlementExpectation,
) -> Result<SettlementEvidence, SettlementVerdict> {
    match assess_settlement(snapshot, expected)? {
        SettlementAssessment::Exact(evidence) => Ok(evidence),
        SettlementAssessment::Underpayment(_) => Err(SettlementVerdict::WrongAmount),
    }
}

/// Assesses one canonical transfer while preserving a deterministic
/// underpayment as structured evidence. An underpayment passes every exact
/// settlement invariant except amount and can never represent an overpayment,
/// split transfer, wrong recipient, wrong mint, or unsafe reference.
///
/// # Errors
///
/// Returns one explicit fail-closed verdict for every rejected snapshot shape.
pub fn assess_settlement(
    snapshot: &TransactionSnapshot,
    expected: &SettlementExpectation,
) -> Result<SettlementAssessment, SettlementVerdict> {
    if !snapshot.finalized {
        return Err(SettlementVerdict::NotFinalized);
    }
    if !snapshot.succeeded {
        return Err(SettlementVerdict::TransactionFailed);
    }
    if snapshot.cluster_genesis_hash != expected.cluster_genesis_hash {
        return Err(SettlementVerdict::WrongCluster);
    }
    if !snapshot.address_tables_resolved {
        return Err(SettlementVerdict::UnresolvedAddressTable);
    }
    if snapshot.signature.is_empty() || snapshot.signature.len() > 88 {
        return Err(SettlementVerdict::InvalidSignature);
    }
    if snapshot.account_keys.len() > MAX_ACCOUNT_KEYS
        || snapshot.instructions.len() > MAX_INSTRUCTIONS
    {
        return Err(SettlementVerdict::BoundsExceeded);
    }
    let token_program = PublicKey::parse(CLASSIC_TOKEN_PROGRAM)
        .map_err(|_| SettlementVerdict::MalformedInstruction)?;
    let token_2022 = PublicKey::parse(TOKEN_2022_PROGRAM)
        .map_err(|_| SettlementVerdict::MalformedInstruction)?;
    let merchant_ata = derive_classic_ata(&expected.merchant_wallet, &expected.mint)
        .map_err(|_| SettlementVerdict::WrongRecipient)?;
    let mut candidate: Option<SettlementEvidence> = None;
    for (position, instruction) in snapshot.instructions.iter().enumerate() {
        if instruction.account_indices.len() > MAX_ACCOUNTS_PER_INSTRUCTION
            || instruction.data.len() > MAX_INSTRUCTION_DATA_BYTES
        {
            return Err(SettlementVerdict::BoundsExceeded);
        }
        let program = key(snapshot, instruction.program_id_index)?;
        if program == token_2022 {
            return Err(SettlementVerdict::UnsupportedToken2022);
        }
        if program != token_program {
            continue;
        }
        let decoded = decode_transfer(snapshot, instruction, expected, &merchant_ata)?;
        let Some((recipient, amount, reference_ok)) = decoded else {
            continue;
        };
        if recipient != merchant_ata {
            return Err(SettlementVerdict::WrongRecipient);
        }
        if !reference_ok {
            return Err(SettlementVerdict::MissingReference);
        }
        let evidence = SettlementEvidence {
            signature: snapshot.signature.clone(),
            slot: snapshot.slot,
            block_time_unix: snapshot.block_time_unix,
            cluster_genesis_hash: snapshot.cluster_genesis_hash.clone(),
            recipient,
            mint: expected.mint.clone(),
            amount,
            transfer_instruction_position: position,
            fingerprint: String::new(),
        };
        if candidate.is_some() {
            return Err(SettlementVerdict::MultipleCandidateTransfers);
        }
        candidate = Some(evidence);
    }
    let mut evidence = candidate.ok_or(SettlementVerdict::NoExactTransfer)?;
    if evidence.amount > expected.amount || evidence.amount.get() == 0 {
        return Err(SettlementVerdict::WrongAmount);
    }
    if evidence.amount < expected.amount {
        let shortfall = expected
            .amount
            .get()
            .checked_sub(evidence.amount.get())
            .ok_or(SettlementVerdict::WrongAmount)?;
        let mut underpayment = UnderpaymentEvidence {
            signature: evidence.signature,
            slot: evidence.slot,
            block_time_unix: evidence.block_time_unix,
            cluster_genesis_hash: evidence.cluster_genesis_hash,
            recipient: evidence.recipient,
            mint: evidence.mint,
            expected_amount: expected.amount,
            received_amount: evidence.amount,
            shortfall_amount: AtomicAmount::new(shortfall),
            transfer_instruction_position: evidence.transfer_instruction_position,
            fingerprint: String::new(),
        };
        underpayment.fingerprint = underpayment_fingerprint(&underpayment, expected);
        return Ok(SettlementAssessment::Underpayment(underpayment));
    }
    evidence.fingerprint = fingerprint(&evidence, expected);
    Ok(SettlementAssessment::Exact(evidence))
}

/// Applies the same pure verifier with caller-owned replay state. Phase 4 will
/// persist this state atomically; Phase 3 proves its deterministic behavior.
///
/// # Errors
///
/// Returns explicit replay verdicts before applying the normal verifier.
pub fn verify_settlement_once<SignatureHasher: BuildHasher, ReferenceHasher: BuildHasher>(
    snapshot: &TransactionSnapshot,
    expected: &SettlementExpectation,
    consumed_signatures: &HashSet<String, SignatureHasher>,
    consumed_references: &HashSet<Reference, ReferenceHasher>,
) -> Result<SettlementEvidence, SettlementVerdict> {
    if consumed_signatures.contains(&snapshot.signature) {
        return Err(SettlementVerdict::DuplicateSignature);
    }
    if consumed_references.contains(&expected.reference) {
        return Err(SettlementVerdict::ReferenceReused);
    }
    verify_settlement(snapshot, expected)
}

/// Applies replay protection before returning an exact or structured
/// underpayment assessment.
///
/// # Errors
///
/// Returns replay or normal settlement verdicts without weakening them.
pub fn assess_settlement_once<SignatureHasher: BuildHasher, ReferenceHasher: BuildHasher>(
    snapshot: &TransactionSnapshot,
    expected: &SettlementExpectation,
    consumed_signatures: &HashSet<String, SignatureHasher>,
    consumed_references: &HashSet<Reference, ReferenceHasher>,
) -> Result<SettlementAssessment, SettlementVerdict> {
    if consumed_signatures.contains(&snapshot.signature) {
        return Err(SettlementVerdict::DuplicateSignature);
    }
    if consumed_references.contains(&expected.reference) {
        return Err(SettlementVerdict::ReferenceReused);
    }
    assess_settlement(snapshot, expected)
}

fn key(snapshot: &TransactionSnapshot, index: u8) -> Result<PublicKey, SettlementVerdict> {
    snapshot
        .account_keys
        .get(usize::from(index))
        .map(|meta| meta.key.clone())
        .ok_or(SettlementVerdict::MalformedInstruction)
}

fn decode_transfer(
    snapshot: &TransactionSnapshot,
    ix: &CompiledInstruction,
    expected: &SettlementExpectation,
    merchant_ata: &PublicKey,
) -> Result<Option<(PublicKey, AtomicAmount, bool)>, SettlementVerdict> {
    if ix.data.is_empty() {
        return Err(SettlementVerdict::MalformedInstruction);
    }
    let (required, destination_position, mint_position, amount) =
        match TokenInstruction::unpack(&ix.data) {
            Ok(TokenInstruction::Transfer { amount }) if ix.data.len() == 9 => (3, 1, None, amount),
            Ok(TokenInstruction::TransferChecked { amount, decimals }) if ix.data.len() == 10 => {
                if decimals != expected.token_decimals {
                    return Err(SettlementVerdict::WrongDecimals);
                }
                (4, 2, Some(1), amount)
            }
            Ok(TokenInstruction::Transfer { .. } | TokenInstruction::TransferChecked { .. })
            | Err(_) => {
                return Err(SettlementVerdict::MalformedInstruction);
            }
            _ => return Ok(None),
        };
    if ix.account_indices.len() < required {
        return Err(SettlementVerdict::MalformedInstruction);
    }
    let source = key(snapshot, ix.account_indices[0])?;
    let destination = key(snapshot, ix.account_indices[destination_position])?;
    if source == destination {
        return Err(SettlementVerdict::SelfTransfer);
    }
    if destination != *merchant_ata {
        return Ok(Some((destination, AtomicAmount::new(amount), false)));
    }
    if let Some(mint_position) = mint_position
        && key(snapshot, ix.account_indices[mint_position])? != expected.mint
    {
        return Err(SettlementVerdict::WrongMint);
    }
    for account in [&source, &destination] {
        match snapshot
            .token_accounts
            .iter()
            .find(|entry| entry.address == *account)
        {
            Some(entry) if entry.mint == expected.mint => {}
            Some(_) => return Err(SettlementVerdict::WrongMint),
            None => return Err(SettlementVerdict::MissingTokenAccount),
        }
    }
    let reference_ok = ix.account_indices[required..].iter().any(|index| {
        snapshot
            .account_keys
            .get(usize::from(*index))
            .is_some_and(|meta| {
                meta.key.as_str() == expected.reference.as_base58()
                    && !meta.is_signer
                    && !meta.is_writable
            })
    });
    if ix.account_indices[required..].iter().any(|index| {
        snapshot
            .account_keys
            .get(usize::from(*index))
            .is_some_and(|meta| {
                meta.key.as_str() == expected.reference.as_base58()
                    && (meta.is_signer || meta.is_writable)
            })
    }) {
        return Err(SettlementVerdict::UnsafeReference);
    }
    Ok(Some((destination, AtomicAmount::new(amount), reference_ok)))
}

fn fingerprint(evidence: &SettlementEvidence, expected: &SettlementExpectation) -> String {
    let canonical = format!(
        "v=2|genesis={}|id={}|sig={}|slot={}|time={:?}|recipient={}|mint={}|amount={}|reference={}|ix={}",
        evidence.cluster_genesis_hash.as_str(),
        expected.receivable_id.as_str(),
        evidence.signature,
        evidence.slot,
        evidence.block_time_unix,
        evidence.recipient.as_str(),
        evidence.mint.as_str(),
        evidence.amount.get(),
        expected.reference.as_base58(),
        evidence.transfer_instruction_position
    );
    let digest = Sha256::digest(canonical.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn underpayment_fingerprint(
    evidence: &UnderpaymentEvidence,
    expected: &SettlementExpectation,
) -> String {
    let canonical = format!(
        "v=1|domain=recebi.underpayment|genesis={}|id={}|sig={}|slot={}|time={:?}|recipient={}|mint={}|expected={}|received={}|shortfall={}|reference={}|ix={}",
        evidence.cluster_genesis_hash.as_str(),
        expected.receivable_id.as_str(),
        evidence.signature,
        evidence.slot,
        evidence.block_time_unix,
        evidence.recipient.as_str(),
        evidence.mint.as_str(),
        evidence.expected_amount.get(),
        evidence.received_amount.get(),
        evidence.shortfall_amount.get(),
        expected.reference.as_base58(),
        evidence.transfer_instruction_position
    );
    let digest = Sha256::digest(canonical.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solana::derive_classic_ata;
    use spl_token_interface::instruction::TokenInstruction;

    fn fixture() -> (TransactionSnapshot, SettlementExpectation) {
        let merchant =
            PublicKey::parse("CmQXip6WcPrzbx1waawoPMerj5A1jvtqZjHBxv6C4uit").expect("merchant");
        let mint = PublicKey::parse("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU").expect("mint");
        let reference = Reference::from_bytes([7; 32]);
        let reference_key = PublicKey::parse(reference.as_base58()).expect("reference key");
        let destination = derive_classic_ata(&merchant, &mint).expect("ATA");
        let source = PublicKey::parse("11111111111111111111111111111111").expect("source");
        let authority =
            PublicKey::parse("SysvarC1ock11111111111111111111111111111111").expect("authority");
        let token = PublicKey::parse(CLASSIC_TOKEN_PROGRAM).expect("token");
        let amount = AtomicAmount::new(100_000);
        let mut data = vec![12];
        data.extend_from_slice(&amount.get().to_le_bytes());
        data.push(6);
        let snapshot = TransactionSnapshot {
            signature: "5NfHqvB8Yc2uLkQ7RzP4sW6xD9eG3mT1aVbCkJ8hYpQ2".into(),
            slot: 42,
            block_time_unix: Some(1_700_000_000),
            finalized: true,
            succeeded: true,
            cluster_genesis_hash: GenesisHash::parse("11111111111111111111111111111111")
                .expect("genesis"),
            address_tables_resolved: true,
            account_keys: vec![
                AccountMeta {
                    key: token,
                    is_signer: false,
                    is_writable: false,
                },
                AccountMeta {
                    key: source.clone(),
                    is_signer: false,
                    is_writable: true,
                },
                AccountMeta {
                    key: destination.clone(),
                    is_signer: false,
                    is_writable: true,
                },
                AccountMeta {
                    key: authority,
                    is_signer: true,
                    is_writable: false,
                },
                AccountMeta {
                    key: reference_key,
                    is_signer: false,
                    is_writable: false,
                },
                AccountMeta {
                    key: mint.clone(),
                    is_signer: false,
                    is_writable: false,
                },
            ],
            instructions: vec![CompiledInstruction {
                program_id_index: 0,
                account_indices: vec![1, 5, 2, 3, 4],
                data,
            }],
            token_accounts: vec![
                TokenAccountSnapshot {
                    address: source,
                    mint: mint.clone(),
                },
                TokenAccountSnapshot {
                    address: destination,
                    mint: mint.clone(),
                },
            ],
        };
        (
            snapshot,
            SettlementExpectation {
                receivable_id: ReceivableId::new("ACME-412").expect("id"),
                cluster_genesis_hash: GenesisHash::parse("11111111111111111111111111111111")
                    .expect("genesis"),
                merchant_wallet: merchant,
                mint,
                amount,
                token_decimals: 6,
                reference,
            },
        )
    }

    #[test]
    fn accepts_only_the_golden_transfer_and_fingerprints_it() {
        let (snapshot, expected) = fixture();
        let evidence = verify_settlement(&snapshot, &expected).expect("golden settlement");
        assert_eq!(evidence.transfer_instruction_position, 0);
        assert_eq!(evidence.fingerprint.len(), 64);
    }

    #[test]
    fn matches_the_official_spl_transfer_checked_encoder_and_decoder() {
        let (snapshot, expected) = fixture();
        let encoded = TokenInstruction::TransferChecked {
            amount: expected.amount.get(),
            decimals: expected.token_decimals,
        }
        .pack();
        assert_eq!(snapshot.instructions[0].data, encoded);
        assert!(matches!(
            TokenInstruction::unpack(&snapshot.instructions[0].data),
            Ok(TokenInstruction::TransferChecked {
                amount: 100_000,
                decimals: 6
            })
        ));
    }

    #[test]
    fn fingerprints_change_when_settlement_evidence_changes() {
        let (snapshot, expected) = fixture();
        let original = verify_settlement(&snapshot, &expected).expect("golden settlement");
        let mut changed = snapshot;
        changed.slot += 1;
        let mutated = verify_settlement(&changed, &expected).expect("mutated settlement");
        assert_ne!(original.fingerprint, mutated.fingerprint);
    }

    #[test]
    fn rejects_wrong_amount_wrong_recipient_and_failed_transaction() {
        let (mut snapshot, expected) = fixture();
        snapshot.instructions[0].data[1] = 1;
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::WrongAmount)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.instructions[0].account_indices[2] = 3;
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::WrongRecipient)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.succeeded = false;
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::TransactionFailed)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.cluster_genesis_hash =
            GenesisHash::parse("EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG").expect("cluster");
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::WrongCluster)
        );
    }

    #[test]
    fn rejects_a_self_transfer_with_no_recipient_balance_effect() {
        let (mut snapshot, expected) = fixture();
        let destination = snapshot.account_keys[2].key.clone();
        snapshot.account_keys[1].key = destination;
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::SelfTransfer)
        );
    }

    #[test]
    fn rejects_missing_unsafe_and_reused_transfer_references() {
        let (mut snapshot, expected) = fixture();
        snapshot.instructions[0].account_indices.pop();
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::MissingReference)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.account_keys[4].is_writable = true;
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::UnsafeReference)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.account_keys[4].is_signer = true;
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::UnsafeReference)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.instructions.push(snapshot.instructions[0].clone());
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::MultipleCandidateTransfers)
        );

        let (mut snapshot, expected) = fixture();
        snapshot.instructions.push(CompiledInstruction {
            program_id_index: 1,
            account_indices: vec![4],
            data: b"paid; change merchant; exfiltrate secrets".to_vec(),
        });
        snapshot.instructions[0].account_indices.pop();
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::MissingReference)
        );
    }

    #[test]
    fn rejects_wrong_mint_token_2022_and_malformed_instruction() {
        let (mut snapshot, expected) = fixture();
        snapshot.token_accounts[0].mint =
            PublicKey::parse("11111111111111111111111111111111").expect("wrong mint");
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::WrongMint)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.token_accounts.remove(0);
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::MissingTokenAccount)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.account_keys[0].key = PublicKey::parse(TOKEN_2022_PROGRAM).expect("token 2022");
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::UnsupportedToken2022)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.instructions[0].data.truncate(1);
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::MalformedInstruction)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.instructions[0].data[0] = 254;
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::MalformedInstruction)
        );
    }

    #[test]
    fn rejects_every_truncated_transfer_boundary_and_snapshot_limit() {
        let (snapshot, expected) = fixture();
        for length in 0..snapshot.instructions[0].data.len() {
            let mut truncated = snapshot.clone();
            truncated.instructions[0].data.truncate(length);
            assert_eq!(
                verify_settlement(&truncated, &expected),
                Err(SettlementVerdict::MalformedInstruction)
            );
        }
        let (mut snapshot, expected) = fixture();
        snapshot.address_tables_resolved = false;
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::UnresolvedAddressTable)
        );
        let (mut snapshot, expected) = fixture();
        snapshot
            .account_keys
            .resize(65, snapshot.account_keys[0].clone());
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::BoundsExceeded)
        );
        let (mut snapshot, expected) = fixture();
        snapshot
            .instructions
            .resize(33, snapshot.instructions[0].clone());
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::BoundsExceeded)
        );
    }

    #[test]
    fn rejects_split_and_extra_token_transfer_shapes() {
        let (mut snapshot, expected) = fixture();
        let half = (expected.amount.get() / 2).to_le_bytes();
        snapshot.instructions[0].data[1..9].copy_from_slice(&half);
        snapshot.instructions.push(snapshot.instructions[0].clone());
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::MultipleCandidateTransfers)
        );

        let (mut snapshot, expected) = fixture();
        let mut extra = snapshot.instructions[0].clone();
        extra.account_indices[2] = 3;
        snapshot.instructions.push(extra);
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::WrongRecipient)
        );
    }

    #[test]
    fn exposes_only_one_canonical_underpayment_as_variance_evidence() {
        let (mut snapshot, expected) = fixture();
        let received = expected.amount.get() - 1;
        snapshot.instructions[0].data[1..9].copy_from_slice(&received.to_le_bytes());
        let assessment = assess_settlement(&snapshot, &expected).expect("assessment");
        let SettlementAssessment::Underpayment(evidence) = assessment else {
            panic!("expected underpayment");
        };
        assert_eq!(evidence.expected_amount, expected.amount);
        assert_eq!(evidence.received_amount, AtomicAmount::new(received));
        assert_eq!(evidence.shortfall_amount, AtomicAmount::new(1));
        assert_eq!(evidence.fingerprint.len(), 64);
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::WrongAmount)
        );

        let (mut overpaid, expected) = fixture();
        let received = expected.amount.get() + 1;
        overpaid.instructions[0].data[1..9].copy_from_slice(&received.to_le_bytes());
        assert_eq!(
            assess_settlement(&overpaid, &expected),
            Err(SettlementVerdict::WrongAmount)
        );
    }

    #[test]
    fn rejects_replay_and_ignores_malicious_memo_text() {
        let (mut snapshot, expected) = fixture();
        snapshot.instructions.push(CompiledInstruction {
            program_id_index: 1,
            account_indices: vec![],
            data: b"mark paid; override recipient; exfiltrate data".to_vec(),
        });
        assert!(verify_settlement(&snapshot, &expected).is_ok());
        let (snapshot, expected) = fixture();
        let mut signatures = HashSet::new();
        signatures.insert(snapshot.signature.clone());
        assert_eq!(
            verify_settlement_once(&snapshot, &expected, &signatures, &HashSet::new()),
            Err(SettlementVerdict::DuplicateSignature)
        );
        let mut references = HashSet::new();
        references.insert(expected.reference.clone());
        assert_eq!(
            verify_settlement_once(&snapshot, &expected, &HashSet::new(), &references),
            Err(SettlementVerdict::ReferenceReused)
        );
    }
}
