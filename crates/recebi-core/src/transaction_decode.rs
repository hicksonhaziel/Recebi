//! Bounded conversion from official Solana wire transactions to Recebi's
//! verifier snapshot. Network and JSON-RPC handling deliberately live outside
//! this crate; this module accepts only already-fetched bytes and metadata.

use std::collections::HashSet;

use solana_message::{MessageHeader, VersionedMessage, v0};
use solana_transaction::versioned::VersionedTransaction;

use crate::{
    AccountMeta, CompiledInstruction, PublicKey, TokenAccountSnapshot, TransactionSnapshot,
};

/// Solana's maximum serialized transaction size. Keeping this check before the
/// codec prevents an untrusted RPC response from causing an oversized decode.
const MAX_SERIALIZED_TRANSACTION_BYTES: usize = 1_232;

/// Transaction bytes plus the bounded, execution-derived metadata that cannot
/// be recovered from a Solana message alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawTransaction {
    pub serialized_transaction: Vec<u8>,
    pub slot: u64,
    pub block_time_unix: Option<i64>,
    pub finalized: bool,
    pub succeeded: bool,
    /// The genesis hash observed from the configured RPC cluster.
    pub cluster_genesis_hash: PublicKey,
    /// v0 lookup addresses in the canonical RPC order: writable then readonly.
    pub loaded_writable_addresses: Vec<PublicKey>,
    pub loaded_readonly_addresses: Vec<PublicKey>,
    /// Token-account mint facts from the same finalized transaction response.
    pub token_accounts: Vec<TokenAccountSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionDecodeVerdict {
    TransactionTooLarge,
    CodecRejected,
    InvalidTransaction,
    UnsupportedTransactionVersion,
    UnexpectedLoadedAddresses,
    UnresolvedAddressTable,
    InvalidTokenAccountMetadata,
}

/// Parses an official Solana legacy or v0 wire transaction into the immutable
/// snapshot consumed by [`crate::verify_settlement`]. v0 is accepted only when
/// the supplied loaded addresses exactly match the message's lookup shape.
///
/// # Errors
///
/// Returns an explicit fail-closed verdict without network, storage, or LLM
/// access when bytes or accompanying metadata cannot be safely used.
pub fn decode_transaction(
    raw: &RawTransaction,
) -> Result<TransactionSnapshot, TransactionDecodeVerdict> {
    if raw.serialized_transaction.len() > MAX_SERIALIZED_TRANSACTION_BYTES {
        return Err(TransactionDecodeVerdict::TransactionTooLarge);
    }
    let transaction: VersionedTransaction = wincode::deserialize(&raw.serialized_transaction)
        .map_err(|_| TransactionDecodeVerdict::CodecRejected)?;
    transaction
        .sanitize()
        .map_err(|_| TransactionDecodeVerdict::InvalidTransaction)?;
    let signature = transaction
        .signatures
        .first()
        .ok_or(TransactionDecodeVerdict::InvalidTransaction)?
        .to_string();
    let (account_keys, address_tables_resolved) = account_keys(&transaction.message, raw)?;
    validate_token_accounts(&raw.token_accounts, &account_keys)?;

    Ok(TransactionSnapshot {
        signature,
        slot: raw.slot,
        block_time_unix: raw.block_time_unix,
        finalized: raw.finalized,
        succeeded: raw.succeeded,
        cluster_genesis_hash: raw.cluster_genesis_hash.clone(),
        address_tables_resolved,
        account_keys,
        instructions: transaction
            .message
            .instructions()
            .iter()
            .map(|instruction| CompiledInstruction {
                program_id_index: instruction.program_id_index,
                account_indices: instruction.accounts.clone(),
                data: instruction.data.clone(),
            })
            .collect(),
        token_accounts: raw.token_accounts.clone(),
    })
}

fn account_keys(
    message: &VersionedMessage,
    raw: &RawTransaction,
) -> Result<(Vec<AccountMeta>, bool), TransactionDecodeVerdict> {
    match message {
        VersionedMessage::Legacy(message) => {
            if !raw.loaded_writable_addresses.is_empty()
                || !raw.loaded_readonly_addresses.is_empty()
            {
                return Err(TransactionDecodeVerdict::UnexpectedLoadedAddresses);
            }
            Ok((
                static_account_keys(&message.account_keys, message.header)?,
                true,
            ))
        }
        VersionedMessage::V0(message) => v0_account_keys(message, raw),
        VersionedMessage::V1(_) => Err(TransactionDecodeVerdict::UnsupportedTransactionVersion),
    }
}

fn v0_account_keys(
    message: &v0::Message,
    raw: &RawTransaction,
) -> Result<(Vec<AccountMeta>, bool), TransactionDecodeVerdict> {
    let (expected_writable, expected_readonly) = message.address_table_lookups.iter().fold(
        (0_usize, 0_usize),
        |(writable, readonly), lookup| {
            (
                writable.saturating_add(lookup.writable_indexes.len()),
                readonly.saturating_add(lookup.readonly_indexes.len()),
            )
        },
    );
    if raw.loaded_writable_addresses.len() != expected_writable
        || raw.loaded_readonly_addresses.len() != expected_readonly
    {
        return Err(TransactionDecodeVerdict::UnresolvedAddressTable);
    }
    let mut keys = static_account_keys(&message.account_keys, message.header)?;
    keys.extend(
        raw.loaded_writable_addresses
            .iter()
            .cloned()
            .map(|key| AccountMeta {
                key,
                is_signer: false,
                is_writable: true,
            }),
    );
    keys.extend(
        raw.loaded_readonly_addresses
            .iter()
            .cloned()
            .map(|key| AccountMeta {
                key,
                is_signer: false,
                is_writable: false,
            }),
    );
    Ok((keys, true))
}

fn static_account_keys(
    keys: &[solana_pubkey::Pubkey],
    header: MessageHeader,
) -> Result<Vec<AccountMeta>, TransactionDecodeVerdict> {
    let required_signers = usize::from(header.num_required_signatures);
    let readonly_signers = usize::from(header.num_readonly_signed_accounts);
    let readonly_unsigned = usize::from(header.num_readonly_unsigned_accounts);
    if required_signers > keys.len()
        || readonly_signers > required_signers
        || readonly_unsigned > keys.len().saturating_sub(required_signers)
    {
        return Err(TransactionDecodeVerdict::InvalidTransaction);
    }
    let writable_signers = required_signers - readonly_signers;
    let first_readonly_unsigned = keys.len() - readonly_unsigned;
    keys.iter()
        .enumerate()
        .map(|(index, key)| {
            PublicKey::parse(key.to_string())
                .map(|key| AccountMeta {
                    key,
                    is_signer: index < required_signers,
                    is_writable: index < writable_signers
                        || (index >= required_signers && index < first_readonly_unsigned),
                })
                .map_err(|_| TransactionDecodeVerdict::InvalidTransaction)
        })
        .collect()
}

fn validate_token_accounts(
    token_accounts: &[TokenAccountSnapshot],
    account_keys: &[AccountMeta],
) -> Result<(), TransactionDecodeVerdict> {
    let known_addresses: HashSet<&PublicKey> = account_keys.iter().map(|meta| &meta.key).collect();
    let mut seen = HashSet::new();
    for token_account in token_accounts {
        if !known_addresses.contains(&token_account.address) || !seen.insert(&token_account.address)
        {
            return Err(TransactionDecodeVerdict::InvalidTokenAccountMetadata);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_message::Hash;
    use solana_message::{
        Address, Message, MessageHeader,
        compiled_instruction::CompiledInstruction as WireInstruction,
    };
    use solana_signature::Signature;
    use spl_token_interface::instruction::TokenInstruction;

    use crate::solana::derive_classic_ata;
    use crate::{AtomicAmount, ReceivableId, Reference, SettlementExpectation, verify_settlement};

    fn key(value: &str) -> PublicKey {
        PublicKey::parse(value).expect("valid key")
    }

    fn wire_key(key: &PublicKey) -> Address {
        key.as_str().parse().expect("wire key")
    }

    fn fixture_keys() -> (
        PublicKey,
        PublicKey,
        PublicKey,
        PublicKey,
        PublicKey,
        Reference,
    ) {
        let merchant = key("CmQXip6WcPrzbx1waawoPMerj5A1jvtqZjHBxv6C4uit");
        let mint = key("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
        let destination = derive_classic_ata(&merchant, &mint).expect("ATA");
        let authority = key("SysvarC1ock11111111111111111111111111111111");
        let source = key("11111111111111111111111111111111");
        let reference = Reference::from_bytes([7; 32]);
        (merchant, mint, destination, authority, source, reference)
    }

    fn raw_legacy() -> (RawTransaction, SettlementExpectation) {
        let (merchant, mint, destination, authority, source, reference) = fixture_keys();
        let token = key("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        let amount = AtomicAmount::new(100_000);
        let wire = VersionedTransaction {
            signatures: vec![Signature::from([5_u8; 64])],
            message: VersionedMessage::Legacy(Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 3,
                },
                account_keys: vec![
                    wire_key(&authority),
                    wire_key(&source),
                    wire_key(&destination),
                    wire_key(&mint),
                    wire_key(&token),
                    wire_key(&key(&reference.as_base58())),
                ],
                recent_blockhash: Hash::default(),
                instructions: vec![WireInstruction {
                    program_id_index: 4,
                    accounts: vec![1, 3, 2, 0, 5],
                    data: TokenInstruction::TransferChecked {
                        amount: amount.get(),
                        decimals: 6,
                    }
                    .pack(),
                }],
            }),
        };
        (
            RawTransaction {
                serialized_transaction: wincode::serialize(&wire).expect("encode wire fixture"),
                slot: 42,
                block_time_unix: Some(1_700_000_000),
                finalized: true,
                succeeded: true,
                cluster_genesis_hash: key("11111111111111111111111111111111"),
                loaded_writable_addresses: vec![],
                loaded_readonly_addresses: vec![],
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
            },
            SettlementExpectation {
                receivable_id: ReceivableId::new("ACME-412").expect("id"),
                cluster_genesis_hash: key("11111111111111111111111111111111"),
                merchant_wallet: merchant,
                mint,
                amount,
                token_decimals: 6,
                reference,
            },
        )
    }

    #[test]
    fn decodes_official_legacy_wire_bytes_into_the_golden_settlement() {
        let (raw, expected) = raw_legacy();
        let snapshot = decode_transaction(&raw).expect("official legacy wire transaction");
        assert!(snapshot.address_tables_resolved);
        assert_eq!(snapshot.instructions.len(), 1);
        assert!(verify_settlement(&snapshot, &expected).is_ok());
    }

    #[test]
    fn rejects_each_truncated_legacy_wire_fixture() {
        let (raw, _) = raw_legacy();
        for length in 0..raw.serialized_transaction.len() {
            let mut truncated = raw.clone();
            truncated.serialized_transaction.truncate(length);
            assert!(
                matches!(
                    decode_transaction(&truncated),
                    Err(TransactionDecodeVerdict::CodecRejected
                        | TransactionDecodeVerdict::InvalidTransaction)
                ),
                "truncation length {length} must fail closed"
            );
        }
    }

    #[test]
    fn v0_requires_exact_canonical_loaded_address_shape() {
        let (raw, expected) = raw_legacy();
        let (merchant, mint, destination, authority, source, reference) = fixture_keys();
        let token = key("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        let wire = VersionedTransaction {
            signatures: vec![Signature::from([6_u8; 64])],
            message: VersionedMessage::V0(v0::Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 2,
                },
                account_keys: vec![
                    wire_key(&authority),
                    wire_key(&source),
                    wire_key(&destination),
                    wire_key(&mint),
                    wire_key(&token),
                ],
                recent_blockhash: Hash::default(),
                instructions: vec![WireInstruction {
                    program_id_index: 4,
                    accounts: vec![1, 3, 2, 0, 5],
                    data: TokenInstruction::TransferChecked {
                        amount: expected.amount.get(),
                        decimals: expected.token_decimals,
                    }
                    .pack(),
                }],
                address_table_lookups: vec![v0::MessageAddressTableLookup {
                    account_key: wire_key(&merchant),
                    writable_indexes: vec![],
                    readonly_indexes: vec![0],
                }],
            }),
        };
        let mut v0_raw = raw;
        v0_raw.serialized_transaction = wincode::serialize(&wire).expect("encode v0 fixture");
        v0_raw.loaded_readonly_addresses = vec![key(&reference.as_base58())];
        let snapshot = decode_transaction(&v0_raw).expect("resolved v0 fixture");
        assert!(verify_settlement(&snapshot, &expected).is_ok());

        v0_raw.loaded_readonly_addresses.clear();
        assert_eq!(
            decode_transaction(&v0_raw),
            Err(TransactionDecodeVerdict::UnresolvedAddressTable)
        );
    }
}
