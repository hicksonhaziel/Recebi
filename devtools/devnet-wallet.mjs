#!/usr/bin/env node

import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { basename, join, resolve } from "node:path";
import {
  Connection,
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  SystemProgram,
  Transaction,
  TransactionInstruction,
  sendAndConfirmTransaction,
} from "@solana/web3.js";

const DEFAULT_RPC = "https://api.devnet.solana.com";
const DEFAULT_MINT = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";
const DEFAULT_DECIMALS = 6;
const TOKEN_PROGRAM_ID = new PublicKey(
  "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
);
const ASSOCIATED_TOKEN_PROGRAM_ID = new PublicKey(
  "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
);
const walletDirectory = resolve(
  process.env.RECEBI_DEVNET_WALLET_DIR ??
    join(homedir(), ".local", "share", "recebi-devnet-payer"),
);
const keypairPath = join(walletDirectory, "keypair.json");

function usage(exitCode = 0) {
  process.stdout.write(`Recebi isolated devnet payer

Usage:
  scripts/devnet-wallet.sh create
  scripts/devnet-wallet.sh reset
  scripts/devnet-wallet.sh address
  scripts/devnet-wallet.sh balance [--mint ADDRESS] [--decimals N] [--rpc HTTPS_URL]
  scripts/devnet-wallet.sh airdrop [SOL] [--rpc HTTPS_URL]
  scripts/devnet-wallet.sh pay --recipient ADDRESS --amount DECIMAL --reference ADDRESS
      [--mint ADDRESS] [--decimals N] [--rpc HTTPS_URL]
  scripts/devnet-wallet.sh self-test

The keypair is stored outside the repository at:
  ${keypairPath}

Devnet testing only. The reset command archives the old key instead of deleting it.
`);
  process.exit(exitCode);
}

function fail(message) {
  process.stderr.write(`Error: ${message}\n`);
  process.exit(1);
}

function parseOptions(arguments_) {
  const options = new Map();
  const positional = [];
  for (let index = 0; index < arguments_.length; index += 1) {
    const value = arguments_[index];
    if (!value.startsWith("--")) {
      positional.push(value);
      continue;
    }
    const next = arguments_[index + 1];
    if (next === undefined || next.startsWith("--")) {
      fail(`missing value for ${value}`);
    }
    options.set(value.slice(2), next);
    index += 1;
  }
  return { options, positional };
}

function option(options, name, fallback) {
  return options.has(name) ? options.get(name) : fallback;
}

function publicKey(value, name) {
  try {
    const key = new PublicKey(value);
    if (key.toBase58() !== value) {
      fail(`${name} must be canonical base58`);
    }
    return key;
  } catch {
    fail(`${name} is not a valid Solana public key`);
  }
}

function decimalsFrom(value) {
  if (!/^(?:0|[1-9][0-9]?)$/.test(String(value))) {
    fail("decimals must be an integer from 0 through 99");
  }
  const decimals = Number(value);
  if (decimals > 18) {
    fail("decimals greater than 18 are not supported by this test tool");
  }
  return decimals;
}

function atomicAmount(value, decimals) {
  if (!/^(?:0|[1-9][0-9]*)(?:\.[0-9]+)?$/.test(value)) {
    fail("amount must be a positive plain decimal");
  }
  const [whole, fraction = ""] = value.split(".");
  if (fraction.length > decimals) {
    fail(`amount exceeds the configured ${decimals}-decimal precision`);
  }
  const atomic = BigInt(whole) * 10n ** BigInt(decimals) +
    BigInt((fraction + "0".repeat(decimals)).slice(0, decimals) || "0");
  if (atomic <= 0n || atomic > 18_446_744_073_709_551_615n) {
    fail("amount is outside the classic SPL u64 range");
  }
  return atomic;
}

function rpcUrl(value) {
  let parsed;
  try {
    parsed = new URL(value);
  } catch {
    fail("RPC URL is invalid");
  }
  if (parsed.protocol !== "https:" || parsed.username || parsed.password) {
    fail("RPC URL must be HTTPS and contain no embedded credentials");
  }
  return parsed.toString();
}

function ensureDirectory() {
  mkdirSync(walletDirectory, { recursive: true, mode: 0o700 });
  chmodSync(walletDirectory, 0o700);
}

function writeKeypair(keypair) {
  ensureDirectory();
  const temporary = join(walletDirectory, `.keypair-${process.pid}.tmp`);
  writeFileSync(temporary, `${JSON.stringify([...keypair.secretKey])}\n`, {
    encoding: "utf8",
    flag: "wx",
    mode: 0o600,
  });
  renameSync(temporary, keypairPath);
  chmodSync(keypairPath, 0o600);
}

function loadKeypair() {
  if (!existsSync(keypairPath)) {
    fail("no test wallet exists; run create first");
  }
  let parsed;
  try {
    parsed = JSON.parse(readFileSync(keypairPath, "utf8"));
  } catch {
    fail("test-wallet keypair file is unreadable");
  }
  if (
    !Array.isArray(parsed) ||
    parsed.length !== 64 ||
    parsed.some((byte) => !Number.isInteger(byte) || byte < 0 || byte > 255)
  ) {
    fail("test-wallet keypair file is malformed");
  }
  chmodSync(keypairPath, 0o600);
  return Keypair.fromSecretKey(Uint8Array.from(parsed));
}

function createWallet() {
  if (existsSync(keypairPath)) {
    const existing = loadKeypair();
    process.stdout.write(`${existing.publicKey.toBase58()}\n`);
    return;
  }
  const created = Keypair.generate();
  writeKeypair(created);
  process.stdout.write(`${created.publicKey.toBase58()}\n`);
}

function resetWallet() {
  ensureDirectory();
  if (existsSync(keypairPath)) {
    const old = loadKeypair();
    const archive = join(walletDirectory, "archive");
    mkdirSync(archive, { recursive: true, mode: 0o700 });
    chmodSync(archive, 0o700);
    const archivedPath = join(
      archive,
      `keypair-${Date.now()}-${old.publicKey.toBase58()}.json`,
    );
    renameSync(keypairPath, archivedPath);
    chmodSync(archivedPath, 0o600);
    process.stderr.write(
      `Archived old wallet ${old.publicKey.toBase58()} as ${basename(archivedPath)}\n`,
    );
  }
  const created = Keypair.generate();
  writeKeypair(created);
  process.stdout.write(`${created.publicKey.toBase58()}\n`);
}

function connectionFrom(options) {
  return new Connection(
    rpcUrl(option(options, "rpc", DEFAULT_RPC)),
    "finalized",
  );
}

function associatedTokenAddress(mint, owner) {
  return PublicKey.findProgramAddressSync(
    [owner.toBuffer(), TOKEN_PROGRAM_ID.toBuffer(), mint.toBuffer()],
    ASSOCIATED_TOKEN_PROGRAM_ID,
  )[0];
}

function createAssociatedTokenAccountIdempotentInstruction(
  payer,
  associatedToken,
  owner,
  mint,
) {
  return new TransactionInstruction({
    programId: ASSOCIATED_TOKEN_PROGRAM_ID,
    keys: [
      { pubkey: payer, isSigner: true, isWritable: true },
      { pubkey: associatedToken, isSigner: false, isWritable: true },
      { pubkey: owner, isSigner: false, isWritable: false },
      { pubkey: mint, isSigner: false, isWritable: false },
      { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    ],
    data: Buffer.from([1]),
  });
}

function createTransferCheckedInstruction(
  source,
  mint,
  destination,
  owner,
  amount,
  decimals,
) {
  const data = Buffer.alloc(10);
  data[0] = 12;
  data.writeBigUInt64LE(amount, 1);
  data[9] = decimals;
  return new TransactionInstruction({
    programId: TOKEN_PROGRAM_ID,
    keys: [
      { pubkey: source, isSigner: false, isWritable: true },
      { pubkey: mint, isSigner: false, isWritable: false },
      { pubkey: destination, isSigner: false, isWritable: true },
      { pubkey: owner, isSigner: true, isWritable: false },
    ],
    data,
  });
}

async function balance(options) {
  const wallet = loadKeypair();
  const connection = connectionFrom(options);
  const mint = publicKey(option(options, "mint", DEFAULT_MINT), "mint");
  const decimals = decimalsFrom(option(options, "decimals", DEFAULT_DECIMALS));
  const tokenAccount = associatedTokenAddress(mint, wallet.publicKey);
  const lamports = await connection.getBalance(wallet.publicKey, "finalized");
  let atomic = 0n;
  try {
    atomic = BigInt(
      (await connection.getTokenAccountBalance(tokenAccount, "finalized")).value
        .amount,
    );
  } catch {
    atomic = 0n;
  }
  process.stdout.write(
    `${JSON.stringify({
      address: wallet.publicKey.toBase58(),
      sol: formatAtomic(BigInt(lamports), 9),
      token_mint: mint.toBase58(),
      token_account: tokenAccount.toBase58(),
      token_balance: formatAtomic(atomic, decimals),
      cluster: "devnet",
    })}\n`,
  );
}

function formatAtomic(atomic, decimals) {
  const padded = atomic.toString().padStart(decimals + 1, "0");
  const whole = padded.slice(0, -decimals) || "0";
  if (decimals === 0) return whole;
  const fraction = padded.slice(-decimals).replace(/0+$/, "");
  return fraction ? `${whole}.${fraction}` : whole;
}

async function airdrop(options, positional) {
  const wallet = loadKeypair();
  const connection = connectionFrom(options);
  const amount = positional[0] ?? "1";
  if (!/^(?:0|[1-9][0-9]*)(?:\.[0-9]+)?$/.test(amount)) {
    fail("airdrop amount must be a plain decimal");
  }
  const lamports = Math.round(Number(amount) * LAMPORTS_PER_SOL);
  if (!Number.isSafeInteger(lamports) || lamports <= 0) {
    fail("airdrop amount is invalid");
  }
  const signature = await connection.requestAirdrop(wallet.publicKey, lamports);
  await connection.confirmTransaction(signature, "finalized");
  process.stdout.write(`${signature}\n`);
}

async function pay(options) {
  for (const required of ["recipient", "amount", "reference"]) {
    if (!options.has(required)) fail(`--${required} is required`);
  }
  const wallet = loadKeypair();
  const connection = connectionFrom(options);
  const recipient = publicKey(options.get("recipient"), "recipient");
  const reference = publicKey(options.get("reference"), "reference");
  const mint = publicKey(option(options, "mint", DEFAULT_MINT), "mint");
  const decimals = decimalsFrom(option(options, "decimals", DEFAULT_DECIMALS));
  const amount = atomicAmount(options.get("amount"), decimals);
  if (recipient.equals(wallet.publicKey)) {
    fail("payer and recipient must be different wallets");
  }
  const source = associatedTokenAddress(mint, wallet.publicKey);
  const destination = associatedTokenAddress(mint, recipient);
  const sourceInfo = await connection.getAccountInfo(source, "finalized");
  if (!sourceInfo || !sourceInfo.owner.equals(TOKEN_PROGRAM_ID)) {
    fail("payer token account does not exist");
  }
  const sourceAmount = BigInt(
    (await connection.getTokenAccountBalance(source, "finalized")).value.amount,
  );
  if (sourceAmount < amount) {
    fail("payer token balance is insufficient");
  }
  const transfer = createTransferCheckedInstruction(
    source,
    mint,
    destination,
    wallet.publicKey,
    amount,
    decimals,
  );
  transfer.keys.push({
    pubkey: reference,
    isSigner: false,
    isWritable: false,
  });
  const transaction = new Transaction().add(
    createAssociatedTokenAccountIdempotentInstruction(
      wallet.publicKey,
      destination,
      recipient,
      mint,
    ),
    transfer,
  );
  const signature = await sendAndConfirmTransaction(
    connection,
    transaction,
    [wallet],
    { commitment: "finalized", preflightCommitment: "confirmed" },
  );
  process.stdout.write(
    `${JSON.stringify({
      signature,
      payer: wallet.publicKey.toBase58(),
      recipient: recipient.toBase58(),
      mint: mint.toBase58(),
      amount: options.get("amount"),
      reference: reference.toBase58(),
      commitment: "finalized",
    })}\n`,
  );
}

function selfTest() {
  const keypair = Keypair.generate();
  if (new PublicKey(keypair.publicKey.toBase58()).toBase58() !== keypair.publicKey.toBase58()) {
    fail("public-key round trip failed");
  }
  if (atomicAmount("0.10", 6) !== 100_000n) {
    fail("amount conversion failed");
  }
  if (formatAtomic(100_000n, 6) !== "0.1") {
    fail("amount formatting failed");
  }
  const mint = publicKey(DEFAULT_MINT, "mint");
  const merchant = publicKey(
    "28GG9LbEzK64zKSreXMn2Bxoy54uDRVT23tMJK6BGasG",
    "merchant",
  );
  const destination = associatedTokenAddress(mint, merchant);
  if (destination.toBase58() !== "tZUfAVm94SosjfM3pAF93fWwKj7bLyCUnjU1enCHq5e") {
    fail("associated-token address derivation failed");
  }
  const transfer = createTransferCheckedInstruction(
    keypair.publicKey,
    mint,
    destination,
    keypair.publicKey,
    100_000n,
    6,
  );
  const reference = Keypair.generate().publicKey;
  transfer.keys.push({
    pubkey: reference,
    isSigner: false,
    isWritable: false,
  });
  if (
    transfer.programId.toBase58() !== TOKEN_PROGRAM_ID.toBase58() ||
    transfer.data.toString("hex") !== "0ca08601000000000006" ||
    transfer.keys.length !== 5 ||
    !transfer.keys[4].pubkey.equals(reference) ||
    transfer.keys[4].isSigner ||
    transfer.keys[4].isWritable
  ) {
    fail("reference-bound TransferChecked encoding failed");
  }
  process.stdout.write("ok\n");
}

async function main() {
  const [command, ...arguments_] = process.argv.slice(2);
  if (!command || command === "help" || command === "--help") usage();
  const { options, positional } = parseOptions(arguments_);
  switch (command) {
    case "create":
      createWallet();
      break;
    case "reset":
      resetWallet();
      break;
    case "address":
      process.stdout.write(`${loadKeypair().publicKey.toBase58()}\n`);
      break;
    case "balance":
      await balance(options);
      break;
    case "airdrop":
      await airdrop(options, positional);
      break;
    case "pay":
      await pay(options);
      break;
    case "self-test":
      selfTest();
      break;
    default:
      usage(1);
  }
}

main().catch((error) => {
  const message = error instanceof Error ? error.message : "unknown failure";
  fail(message.replaceAll(/\s+/g, " ").slice(0, 300));
});
