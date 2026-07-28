use std::{collections::HashSet, hash::BuildHasher};

use sha2::{Digest, Sha256};

use crate::{AtomicAmount, PublicKey, ReceivableId, Reference, solana::derive_classic_ata};

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
    pub address_tables_resolved: bool,
    pub account_keys: Vec<AccountMeta>,
    pub instructions: Vec<CompiledInstruction>,
    pub token_accounts: Vec<TokenAccountSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlementExpectation {
    pub receivable_id: ReceivableId,
    pub merchant_wallet: PublicKey,
    pub mint: PublicKey,
    pub amount: AtomicAmount,
    pub reference: Reference,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlementEvidence {
    pub signature: String,
    pub slot: u64,
    pub block_time_unix: Option<i64>,
    pub recipient: PublicKey,
    pub mint: PublicKey,
    pub amount: AtomicAmount,
    pub transfer_instruction_position: usize,
    pub fingerprint: String,
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
    WrongRecipient,
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
/// # Panics
///
/// Panics only if a compile-time Solana program-address constant is malformed.
pub fn verify_settlement(
    snapshot: &TransactionSnapshot,
    expected: &SettlementExpectation,
) -> Result<SettlementEvidence, SettlementVerdict> {
    if !snapshot.finalized {
        return Err(SettlementVerdict::NotFinalized);
    }
    if !snapshot.succeeded {
        return Err(SettlementVerdict::TransactionFailed);
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
    let token_program = PublicKey::parse(CLASSIC_TOKEN_PROGRAM).expect("constant is valid");
    let token_2022 = PublicKey::parse(TOKEN_2022_PROGRAM).expect("constant is valid");
    let merchant_ata = derive_classic_ata(&expected.merchant_wallet, &expected.mint)
        .map_err(|_| SettlementVerdict::WrongRecipient)?;
    let mut exact: Option<SettlementEvidence> = None;
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
        if amount != expected.amount {
            return Err(SettlementVerdict::WrongAmount);
        }
        let evidence = SettlementEvidence {
            signature: snapshot.signature.clone(),
            slot: snapshot.slot,
            block_time_unix: snapshot.block_time_unix,
            recipient,
            mint: expected.mint.clone(),
            amount,
            transfer_instruction_position: position,
            fingerprint: String::new(),
        };
        if exact.is_some() {
            return Err(SettlementVerdict::MultipleCandidateTransfers);
        }
        exact = Some(evidence);
    }
    let mut evidence = exact.ok_or(SettlementVerdict::NoExactTransfer)?;
    evidence.fingerprint = fingerprint(&evidence, expected);
    Ok(evidence)
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
    let Some((&opcode, data)) = ix.data.split_first() else {
        return Err(SettlementVerdict::MalformedInstruction);
    };
    let (required, destination_position, mint_position, amount) = match opcode {
        3 if data.len() == 8 => (
            3,
            1,
            None,
            u64::from_le_bytes(
                data.try_into()
                    .map_err(|_| SettlementVerdict::MalformedInstruction)?,
            ),
        ),
        12 if data.len() == 9 => (
            4,
            2,
            Some(1),
            u64::from_le_bytes(
                data[..8]
                    .try_into()
                    .map_err(|_| SettlementVerdict::MalformedInstruction)?,
            ),
        ),
        3 | 12 => return Err(SettlementVerdict::MalformedInstruction),
        _ => return Ok(None),
    };
    if ix.account_indices.len() < required {
        return Err(SettlementVerdict::MalformedInstruction);
    }
    let source = key(snapshot, ix.account_indices[0])?;
    let destination = key(snapshot, ix.account_indices[destination_position])?;
    if destination != *merchant_ata {
        return Ok(Some((destination, AtomicAmount::new(amount), false)));
    }
    if let Some(mint_position) = mint_position
        && key(snapshot, ix.account_indices[mint_position])? != expected.mint
    {
        return Err(SettlementVerdict::WrongMint);
    }
    for account in [&source, &destination] {
        if snapshot
            .token_accounts
            .iter()
            .find(|entry| entry.address == *account)
            .map(|entry| &entry.mint)
            != Some(&expected.mint)
        {
            return Err(SettlementVerdict::WrongMint);
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
        "v=1|id={}|sig={}|slot={}|time={:?}|recipient={}|mint={}|amount={}|reference={}|ix={}",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solana::derive_classic_ata;

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
                merchant_wallet: merchant,
                mint,
                amount,
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
    fn rejects_wrong_amount_wrong_recipient_and_failed_transaction() {
        let (mut snapshot, expected) = fixture();
        snapshot.instructions[0].data[1] = 1;
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::WrongAmount)
        );
        let (mut snapshot, expected) = fixture();
        snapshot.instructions[0].account_indices[2] = 1;
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
        snapshot.instructions.push(snapshot.instructions[0].clone());
        assert_eq!(
            verify_settlement(&snapshot, &expected),
            Err(SettlementVerdict::MultipleCandidateTransfers)
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
