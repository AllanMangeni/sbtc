//! This is the transaction analysis module
//!

use bitcoin::opcodes::all as opcodes;
use bitcoin::script::PushBytesBuf;
use bitcoin::taproot::LeafVersion;
use bitcoin::taproot::NodeInfo;
use bitcoin::taproot::TaprootSpendInfo;
use bitcoin::Address;
use bitcoin::Network;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use bitcoin::Transaction;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use clarity::codec::StacksMessageCodec;
use clarity::vm::types::PrincipalData;
use secp256k1::SECP256K1;
use stacks_common::types::chainstate::STACKS_ADDRESS_ENCODED_SIZE;

/// This is the length of the fixed portion of the deposit script, which
/// is:
/// ```text
///  OP_DROP OP_PUSHBYTES_32 <x-only-public-key> OP_CHECKSIG
/// ```
/// Since we are using Schnorr signatures, we only use the x-coordinate of
/// the public key. The full public key is assumed to be even.
const DEPOSIT_SCRIPT_FIXED_LENGTH: usize = 35;

/// This is the typical number of bytes of a deposit script. It's 1 byte
/// for the length of the following 30 bytes of data, which is 8 bytes for
/// the max fee followed by 1 byte for the address type, 21 bytes the
/// actual standard stacks address, followed by 34 bytes for the fixed
/// length portion of the deposit script. So we have the standard length is
/// 1 + 1 + 8 + 21 + 34 = 65.
const STANDARD_SCRIPT_LENGTH: usize =
    1 + 1 + 8 + STACKS_ADDRESS_ENCODED_SIZE as usize + DEPOSIT_SCRIPT_FIXED_LENGTH;

/// Script opcodes as the bytes in bitcoin Script.
///
/// Drops the top stack item
const OP_DROP: u8 = opcodes::OP_DROP.to_u8();
/// <https://en.bitcoin.it/wiki/OP_CHECKSIG> pushing 1/0 for
/// success/failure.
const OP_CHECKSIG: u8 = opcodes::OP_CHECKSIG.to_u8();
/// Read the next byte as N; push the next N bytes as an array onto the
/// stack.
const OP_PUSHDATA1: u8 = opcodes::OP_PUSHDATA1.to_u8();
/// The OP_CHECKSEQUENCEVERIFY opcode described in
/// https://github.com/bitcoin/bips/blob/master/bip-0112.mediawiki
const OP_CSV: u8 = opcodes::OP_CSV.to_u8();
/// Represents the number 1.
const OP_PUSHNUM_1: u8 = opcodes::OP_PUSHNUM_1.to_u8();
/// Represents the number 16.
const OP_PUSHNUM_16: u8 = opcodes::OP_PUSHNUM_16.to_u8();
/// Represents the number -1.
const OP_PUSHNUM_NEG1: u8 = opcodes::OP_PUSHNUM_NEG1.to_u8();

/// Errors
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The deposit script was invalid
    #[error("invalid deposit script")]
    InvalidDepositScript,
    /// The lock time included in the reclaim script was invalid.
    #[error("the lock time included in the reclaim script was invalid: {0}")]
    InvalidReclaimScriptLockTime(i64),
    /// The reclaim script was invalid.
    #[error("the reclaim script format was invalid")]
    InvalidReclaimScript,
    /// The reclaim script lock time was invalid
    #[error("reclaim script lock time was either too large or non-minimal: {0}")]
    ScriptNum(#[source] bitcoin::script::Error),
    /// The X-only public key was invalid
    #[error("the x-only public key in the script was invalid: {0}")]
    InvalidXOnlyPublicKey(#[source] secp256k1::Error),
    /// Could not parse the Stacks principal address.
    #[error("could not parse the stacks principal address: {0}")]
    ParseStacksAddress(#[source] stacks_common::codec::Error),
    /// Failed to parse the hex as a bitcoin::Transaction.
    #[error("could not parse the BTC transaction hex: {0}")]
    DecodeFromHex(#[source] bitcoin::consensus::encode::FromHexError),
    /// Failed to extract the outpoint from the bitcoin::Transaction.
    #[error("could not get outpoint {1} from BTC transaction: {0}")]
    OutpointIndex(
        #[source] bitcoin::blockdata::transaction::OutputsIndexError,
        OutPoint,
    ),
    /// The ScriptPubKey of the UTXO did not match what was expected from
    /// the given deposit script and reclaim script.
    #[error("mismatch in expected and actual ScriptPubKeys. outpoint: {0}")]
    UtxoScriptPubKeyMismatch(OutPoint),
    /// Failed to parse the hex as a bitcoin::Transaction.
    #[error("could not parse the bitcoin transaction hex")]
    TxidMismatch {
        /// This is the transaction ID of the actual transaction
        from_tx: Txid,
        /// This is the transaction ID of from the request
        from_request: Txid,
    },
}

/// All the info required to verify the validity of a deposit
/// transaction. This info is sent by the user to the Emily API
pub struct CreateDepositRequest {
    /// The output index and txid of the depositing transaction.
    pub outpoint: OutPoint,
    /// The raw reclaim script.
    pub reclaim_script: ScriptBuf,
    /// The raw deposit script.
    pub deposit_script: ScriptBuf,
}

/// All the deposit script with the relevant parts of the deposit and
/// reclaim scripts parsed.
#[derive(Debug, Clone)]
pub struct ParsedDepositRequest {
    /// The UTXO to be spent by the signers.
    pub outpoint: OutPoint,
    /// The max fee amount to use for the BTC deposit transaction.
    pub max_fee: u64,
    /// The amount of sats in the deposit UTXO.
    pub amount: u64,
    /// The deposit script used so that the signers' can spend funds.
    pub deposit_script: ScriptBuf,
    /// The reclaim script for the deposit.
    pub reclaim_script: ScriptBuf,
    /// The public key used in the deposit script. The signers public key
    /// is for Schnorr signatures.
    pub signers_public_key: XOnlyPublicKey,
    /// The stacks address to deposit the sBTC to. This can be either a
    /// standard address or a contract address.
    pub recipient: PrincipalData,
    /// The relative lock time in the reclaim script.
    pub lock_time: u64,
}

impl CreateDepositRequest {
    /// Validate the deposit request.
    ///
    /// This function checks the following
    /// * That the provided tx hex is a valid transaction.
    /// * That the transaction's txid matches the expected txid from the
    ///   request.
    /// * That the expected UTXO is in the transaction.
    /// * That the deposit script and reclaim script in the request match
    ///   the expected formats for deposit transactions.
    /// * That deposit script and the reclaim script are part of the UTXO
    ///   ScriptPubKey.
    pub fn validate_tx(&self, tx_hex: &str) -> Result<ParsedDepositRequest, Error> {
        let tx: Transaction =
            bitcoin::consensus::encode::deserialize_hex(tx_hex).map_err(Error::DecodeFromHex)?;

        if tx.compute_txid() != self.outpoint.txid {
            // The expectation is that the transaction hex was fetched from
            // the blockchain using the txid, so in practice this should
            // never happen.
            return Err(Error::TxidMismatch {
                from_request: self.outpoint.txid,
                from_tx: tx.compute_txid(),
            });
        }

        let tx_out = tx
            .tx_out(self.outpoint.vout as usize)
            .map_err(|err| Error::OutpointIndex(err, self.outpoint))?;
        // Validate that the deposit and reclaim scripts in the request
        // match the expected formats for deposit transactions.
        let deposit = DepositScriptInputs::parse(&self.deposit_script)?;
        let reclaim = ReclaimScriptInputs::parse(&self.reclaim_script)?;
        // Okay, the deposit and reclaim scripts are valid. Now make sure
        // that the ScriptPubKey in the transaction matches the one implied
        // by the given scripts. So now create the expected ScriptPubKey.
        let deposit_script = deposit.deposit_script();
        let reclaim_script = reclaim.reclaim_script();

        debug_assert_eq!(deposit_script, self.deposit_script);
        debug_assert_eq!(reclaim_script, self.reclaim_script);

        let expected_script_pubkey = to_script_pubkey(deposit_script, reclaim_script);
        // Check that the expected scriptPubkey matches the actual public
        // key of our parsed UTXO.
        if expected_script_pubkey != tx_out.script_pubkey {
            return Err(Error::UtxoScriptPubKeyMismatch(self.outpoint));
        }

        Ok(ParsedDepositRequest {
            max_fee: deposit.max_fee,
            deposit_script: self.deposit_script.clone(),
            reclaim_script: self.reclaim_script.clone(),
            signers_public_key: deposit.signers_public_key,
            recipient: deposit.recipient,
            lock_time: reclaim.lock_time as u64,
            amount: tx_out.value.to_sat(),
            outpoint: self.outpoint,
        })
    }
}

/// Construct the expected taproot info for a deposit UTXO on the given
/// the deposit and reclaim scripts.
pub fn to_taproot(deposit_script: ScriptBuf, reclaim_script: ScriptBuf) -> TaprootSpendInfo {
    let ver = LeafVersion::TapScript;
    // For such a simple tree, we construct it by hand.
    let leaf1 = NodeInfo::new_leaf_with_ver(deposit_script, ver);
    let leaf2 = NodeInfo::new_leaf_with_ver(reclaim_script, ver);
    // A Result::Err is returned by NodeInfo::combine if the depth of
    // our taproot tree exceeds the maximum depth of taproot trees,
    // which is 128. We have two nodes so the depth is 1 so this will
    // never panic.
    let node = NodeInfo::combine(leaf1, leaf2).expect("Tree depth is greater than the max of 128");
    let internal_key = crate::unspendable_taproot_key();

    TaprootSpendInfo::from_node_info(SECP256K1, *internal_key, node)
}

/// Create the expected ScriptPubKey from the deposit and reclaim scripts.
pub fn to_script_pubkey(deposit_script: ScriptBuf, reclaim_script: ScriptBuf) -> ScriptBuf {
    let merkle_root = to_taproot(deposit_script, reclaim_script).merkle_root();
    // Deposit transactions use a NUMS (nothing up my sleeve) public
    // key for the key-spend path of taproot scripts.
    let internal_key = crate::unspendable_taproot_key();
    ScriptBuf::new_p2tr(SECP256K1, *internal_key, merkle_root)
}

/// Construct a bitcoin address for a deposit UTXO on the given
/// network.
fn p2tr_address(deposit_script: ScriptBuf, reclaim_script: ScriptBuf, network: Network) -> Address {
    let internal_key = crate::unspendable_taproot_key();
    let merkle_root = to_taproot(deposit_script, reclaim_script).merkle_root();
    Address::p2tr(SECP256K1, *internal_key, merkle_root, network)
}

/// This struct contains the key variable inputs when constructing a
/// deposit script address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositScriptInputs {
    /// The last known public key of the signers.
    pub signers_public_key: XOnlyPublicKey,
    /// The stacks address to deposit the sBTC to. This can be either a
    /// standard address or a contract address.
    pub recipient: PrincipalData,
    /// The max fee amount to use for the BTC deposit transaction.
    pub max_fee: u64,
}

impl DepositScriptInputs {
    /// Construct a bitcoin address for a deposit UTXO on the given
    /// network and reclaim script.
    pub fn to_address(&self, reclaim_script: ScriptBuf, network: Network) -> Address {
        let deposit_script = self.deposit_script();
        p2tr_address(deposit_script, reclaim_script, network)
    }

    /// Construct a deposit script from the inputs
    pub fn deposit_script(&self) -> ScriptBuf {
        // The format of the OP_DROP data, as shown in
        // https://github.com/stacks-network/sbtc/issues/30, is 8 bytes for
        // the max fee followed by up to 151 bytes for the stacks address.
        let recipient_bytes = self.recipient.serialize_to_vec();
        let mut op_drop_data = PushBytesBuf::with_capacity(recipient_bytes.len() + 8);
        // These should never fail. The PushBytesBuf type only
        // errors if the total length of the buffer is greater than
        // u32::MAX. We're pushing a max of 159 bytes.
        op_drop_data
            .extend_from_slice(&self.max_fee.to_be_bytes())
            .expect("8 is greater than u32::MAX?");
        op_drop_data
            .extend_from_slice(&recipient_bytes)
            .expect("159 is greater than u32::MAX?");
        // When using the bitcoin::script::Builder, push_slice
        // automatically inserts the appropriate opcodes based on the data
        // size to be pushed onto the stack. Here, OP_PUSHBYTES_32 is
        // pushed before the public key. Also, OP_PUSHBYTES_N is used if
        // the OP_DROP data length is between 1 and 75 otherwise
        // OP_PUSHDATA1 is used since the data length is less than 255.
        ScriptBuf::builder()
            .push_slice(op_drop_data)
            .push_opcode(opcodes::OP_DROP)
            .push_slice(self.signers_public_key.serialize())
            .push_opcode(opcodes::OP_CHECKSIG)
            .into_script()
    }

    /// This function checks that the deposit script is valid.
    ///
    /// Specifically, it checks that it follows the format laid out in
    /// https://github.com/stacks-network/sbtc/issues/30, where the script
    /// is expected to be
    /// ```text
    ///  <deposit-data> OP_DROP OP_PUSHBYTES_32 <x-only-public-key> OP_CHECKSIG
    /// ```
    /// The <deposit-data> is expected to have the format
    /// <max-fee><recipient-address>, where the recipient address follows
    /// the format for a principal from SIP-005. So the expected wire
    /// format is:
    ///
    /// 0         8             9         10        30            31             159
    /// |---------|-------------|---------|---------|-------------|---------------|
    ///   max fee   type prefix   version   hash160   name length   contract name
    ///                                               (optional)    (optional)
    ///
    /// Above, the max fee is expressed as an 8-byte big endian integer and
    /// the contract name is a UTF-8 encoded string and must be accepted by
    /// the regex `^[a-zA-Z]([a-zA-Z0-9]|[-_])*$`.
    ///
    /// SIP-005:
    /// https://github.com/stacksgov/sips/blob/0b19b15a9f2dd43caf6607de4fe53cad8313ff40/sips/sip-005/sip-005-blocks-and-transactions.md#transaction-post-conditions
    pub fn parse(deposit_script: &ScriptBuf) -> Result<Self, Error> {
        let script = deposit_script.as_bytes();

        // Valid deposit scripts cannot be less than this length.
        if script.len() < STANDARD_SCRIPT_LENGTH {
            return Err(Error::InvalidDepositScript);
        }
        // This cannot panic because of the above check and the fact that
        // DEPOSIT_SCRIPT_FIXED_LENGTH < STANDARD_SCRIPT_LENGTH.
        let (params, check) = script.split_at(script.len() - DEPOSIT_SCRIPT_FIXED_LENGTH);
        // Below, we know the script length is DEPOSIT_SCRIPT_FIXED_LENGTH,
        // because of how `slice::split_at` works, so we know the
        // public_key variable has length 32.
        let [OP_DROP, 32, public_key @ .., OP_CHECKSIG] = check else {
            return Err(Error::InvalidDepositScript);
        };
        // In bitcoin script, the code for pushing N bytes onto the stack
        // is OP_PUSHBYTES_N where N is between 1 and 75 inclusive. The
        // byte representation of these opcodes is the byte representation
        // of N. If you need to push between 76 and 255 bytes of data then
        // you need to use the OP_PUSHDATA1 opcode (you can also use this
        // opcode to push between 1 and 75 bytes on the stack, but it's
        // non-standard and cheaper to use the OP_PUSHBYTES_N opcodes when
        // you can). We need to check all cases since contract addresses
        // can have a size of up to 151 bytes. Note that the data slice
        // here starts with the 8 byte max fee.
        let deposit_data = match params {
            // This branch represents a contract address.
            [OP_PUSHDATA1, n, data @ ..] if data.len() == *n as usize && *n < 160 => data,
            // This branch can be a standard (non-contract) Stacks
            // addresses when n == 30 (8 byte max fee + 22 byte address)
            // and is a contract address otherwise.
            [n, data @ ..] if data.len() == *n as usize && *n < 76 => data,
            _ => return Err(Error::InvalidDepositScript),
        };
        // Here `split_first_chunk::<N>` returns Option<(&[u8; N], &[u8])>,
        // where None is returned if the length of the slice is less than
        // N. Since N is 8 and the data variable has a length 30 or
        // greater, the error path cannot happen.
        let Some((max_fee_bytes, mut address)) = deposit_data.split_first_chunk::<8>() else {
            return Err(Error::InvalidDepositScript);
        };
        let stacks_address = PrincipalData::consensus_deserialize(&mut address)
            .map_err(Error::ParseStacksAddress)?;

        Ok(DepositScriptInputs {
            signers_public_key: XOnlyPublicKey::from_slice(public_key)
                .map_err(Error::InvalidXOnlyPublicKey)?,
            max_fee: u64::from_be_bytes(*max_fee_bytes),
            recipient: stacks_address,
        })
    }
}
/// This struct contains the key variable inputs when constructing a
/// deposit script address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimScriptInputs {
    /// This is the lock time used for the OP_CSV opcode in the reclaim
    /// script.
    lock_time: i64,
    /// The reclaim script after the <locked-time> OP_CSV part of the script
    script: ScriptBuf,
}

impl ReclaimScriptInputs {
    /// Create a new one
    pub fn try_new(lock_time: i64, script: ScriptBuf) -> Result<Self, Error> {
        // We can only use numbers that can be expressed as a 5-byte signed
        // integer, which has a max of 2**39 - 1. Negative numbers might be
        // considered non-standard, so we reject them as well.
        if lock_time > i64::pow(2, 39) - 1 || lock_time < 0 {
            return Err(Error::InvalidReclaimScriptLockTime(lock_time));
        }

        Ok(Self { lock_time, script })
    }

    /// Create the reclaim script from the inputs
    pub fn reclaim_script(&self) -> ScriptBuf {
        let mut lock_script = ScriptBuf::builder()
            .push_int(self.lock_time)
            .push_opcode(opcodes::OP_CSV)
            .into_script()
            .into_bytes();

        lock_script.extend(self.script.as_bytes());
        ScriptBuf::from_bytes(lock_script)
    }

    /// Parse the reclaim script for the lock time.
    ///
    /// The goal of this function is to make sure that there are no
    /// surprises in the reclaim script. These scripts are conceptually
    /// very simple and are their format is
    /// ```text
    ///  <locked-time> OP_CHECKSEQUENCEVERIFY <rest-of-reclaim-script>
    /// ```
    /// This function extracts the <locked-time> from the script. If the
    /// script does not start with <locked-time> OP_CHECKSEQUENCEVERIFY
    /// then we return an error.
    ///
    /// See https://github.com/stacks-network/sbtc/issues/30 for the
    /// expected format of the reclaim script. And see BIP-0112 for
    /// the details and input conditions of OP_CHECKSEQUENCEVERIFY:
    /// https://github.com/bitcoin/bips/blob/812907c2b00b92ee31e2b638622a4fe14a428aee/bip-0112.mediawiki#summary
    pub fn parse(reclaim_script: &ScriptBuf) -> Result<Self, Error> {
        let (lock_time, script) = match reclaim_script.as_bytes() {
            // These first two branches check for the case when the script
            // is written with as few bytes as possible (called minimal
            // CScriptNum format or something like that).
            [0, OP_CSV, script @ ..] => (0, script),
            // This catches numbers 1-16 and -1. Negative numbers are
            // invalid for OP_CHECKSEQUENCEVERIFY, but we filter them out
            // later in `ReclaimScriptInputs::try_new`.
            [n, OP_CSV, script @ ..]
                if OP_PUSHNUM_NEG1 == *n || (OP_PUSHNUM_1..OP_PUSHNUM_16).contains(n) =>
            {
                (*n as i64 - OP_PUSHNUM_1 as i64 + 1, script)
            }
            // Numbers in bitcoin script are typically only 4 bytes (with a
            // range from -2**31+1 to 2**31-1), unless we are working with
            // OP_CSV or OP_CLTV, where 5-byte numbers are acceptable (with
            // a range of 0 to 2**39-1). See the following for how the code
            // works in bitcoin-core:
            // https://github.com/bitcoin/bitcoin/blob/v27.1/src/script/interpreter.cpp#L531-L573
            [n, rest @ ..] if *n <= 5 && rest.get(*n as usize) == Some(&OP_CSV) => {
                // We know the error and panic paths cannot happen because
                // of the above `if` check.
                let (script_num, [OP_CSV, script @ ..]) = rest.split_at(*n as usize) else {
                    return Err(Error::InvalidDepositScript);
                };
                (read_scriptint(script_num, 5)?, script)
            }
            _ => return Err(Error::InvalidReclaimScript),
        };

        let script = ScriptBuf::from_bytes(script.to_vec());
        ReclaimScriptInputs::try_new(lock_time, script)
    }
}

/// Decodes an integer in script(minimal CScriptNum) format.
///
/// # Notes
///
/// This code is a slightly modified version of the code in rust-bitcoin:
/// https://github.com/rust-bitcoin/rust-bitcoin/blob/bitcoin-0.32.2/bitcoin/src/blockdata/script/mod.rs#L158-L200.
///
/// The logic here and in rust-bitcoin are both based on the `CScriptNum`
/// constructor in Bitcoin Core:
/// https://github.com/bitcoin/bitcoin/blob/v27.1/src/script/script.h#L244C1-L269C6
fn read_scriptint(v: &[u8], max_size: usize) -> Result<i64, Error> {
    let last = match v.last() {
        Some(last) => last,
        None => return Ok(0),
    };
    // In rust-bitcoin, max_size is hardcoded to 4, while in bitcoin-core
    // it is a variable, and sometimes they set it to 5. This is the only
    // modification to this function body from rust-bitcoin's code.
    if v.len() > max_size {
        return Err(Error::ScriptNum(bitcoin::script::Error::NumericOverflow));
    }
    // Comment and code copied from Bitcoin Core:
    // https://github.com/bitcoin/bitcoin/blob/447f50e4aed9a8b1d80e1891cda85801aeb80b4e/src/script/script.h#L247-L262
    // If the most-significant-byte - excluding the sign bit - is zero
    // then we're not minimal. Note how this test also rejects the
    // negative-zero encoding, 0x80.
    if (*last & 0x7f) == 0 {
        // One exception: if there's more than one byte and the most
        // significant bit of the second-most-significant-byte is set
        // it would conflict with the sign bit. An example of this case
        // is +-255, which encode to 0xff00 and 0xff80 respectively.
        // (big-endian).
        if v.len() <= 1 || (v[v.len() - 2] & 0x80) == 0 {
            return Err(Error::ScriptNum(bitcoin::script::Error::NonMinimalPush));
        }
    }

    Ok(scriptint_parse(v))
}

/// Caller to guarantee that `v` is not empty.
///
/// # Notes
///
/// This code was lifted from rust-bitcoin without modification:
/// https://github.com/rust-bitcoin/rust-bitcoin/blob/bitcoin-0.32.2/bitcoin/src/blockdata/script/mod.rs#L218-L226C2.
///
/// The logic there follows the logic in bitcoin-core:
/// https://github.com/bitcoin/bitcoin/blob/v27.1/src/script/script.h#L382C1-L400C3
fn scriptint_parse(v: &[u8]) -> i64 {
    let (mut ret, sh) = v
        .iter()
        .fold((0, 0), |(acc, sh), n| (acc + ((*n as i64) << sh), sh + 8));
    if v[v.len() - 1] & 0x80 != 0 {
        ret &= (1 << (sh - 1)) - 1;
        ret = -ret;
    }
    ret
}

#[cfg(test)]
mod tests {
    use bitcoin::absolute::LockTime;
    use bitcoin::hashes::Hash as _;
    use bitcoin::transaction::Version;
    use bitcoin::AddressType;
    use bitcoin::Amount;
    use bitcoin::TxOut;
    use rand::rngs::OsRng;
    use secp256k1::SecretKey;
    use stacks_common::codec::StacksMessageCodec;
    use stacks_common::types::chainstate::StacksAddress;

    use super::*;

    use test_case::test_case;

    const CONTRACT_ADDRESS: &str = "ST1RQHF4VE5CZ6EK3MZPZVQBA0JVSMM9H5PMHMS1Y.contract-name";

    /// A full reclaim script with a p2pk script at the end.
    fn reclaim_p2pk(lock_time: i64) -> ScriptBuf {
        ScriptBuf::builder()
            .push_int(lock_time)
            .push_opcode(opcodes::OP_CSV)
            .push_opcode(opcodes::OP_DROP)
            .push_slice([0; 32])
            .push_opcode(opcodes::OP_CHECKSIG)
            .into_script()
    }

    struct TxSetup {
        tx: Transaction,
        deposit: DepositScriptInputs,
        reclaim: ReclaimScriptInputs,
    }

    // The BTC transaction that is in this TxSetup is consistent with
    // the deposit and reclaim scripts.
    fn tx_setup(lock_time: i64, max_fee: u64, amount: u64) -> TxSetup {
        let secret_key = SecretKey::new(&mut OsRng);

        let deposit = DepositScriptInputs {
            signers_public_key: secret_key.x_only_public_key(SECP256K1).0,
            recipient: PrincipalData::from(StacksAddress::burn_address(false)),
            max_fee,
        };
        let reclaim = ReclaimScriptInputs::try_new(lock_time, ScriptBuf::new()).unwrap();

        let deposit_script = deposit.deposit_script();
        let reclaim_script = reclaim.reclaim_script();
        // This transaction is kinda invalid because it doesn't have any
        // inputs. But it is fine for our purposes.
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: Vec::new(),
            output: vec![TxOut {
                value: Amount::from_sat(amount),
                script_pubkey: to_script_pubkey(deposit_script, reclaim_script),
            }],
        };

        TxSetup { tx, reclaim, deposit }
    }

    /// Check that manually creating the expected script can correctly be
    /// parsed.
    #[test_case(PrincipalData::from(StacksAddress::burn_address(false)) ; "standard address")]
    #[test_case(PrincipalData::parse(CONTRACT_ADDRESS).unwrap(); "contract address")]
    fn deposit_script_parsing_works_standard_principal(recipient: PrincipalData) {
        let secret_key = SecretKey::new(&mut OsRng);
        let public_key = secret_key.x_only_public_key(SECP256K1).0;
        let max_fee: u64 = 15000;

        let mut deposit_data = max_fee.to_be_bytes().to_vec();
        deposit_data.extend_from_slice(&recipient.serialize_to_vec());

        let deposit_data: PushBytesBuf = deposit_data.try_into().unwrap();

        let script = ScriptBuf::builder()
            .push_slice(deposit_data)
            .push_opcode(opcodes::OP_DROP)
            .push_slice(public_key.serialize())
            .push_opcode(opcodes::OP_CHECKSIG)
            .into_script();

        if matches!(recipient, PrincipalData::Standard(_)) {
            assert_eq!(script.len(), STANDARD_SCRIPT_LENGTH);
        }

        let extracts = DepositScriptInputs::parse(&script).unwrap();
        assert_eq!(extracts.signers_public_key, public_key);
        assert_eq!(extracts.recipient, recipient);
        assert_eq!(extracts.max_fee, max_fee);
        assert_eq!(extracts.deposit_script(), script);
    }

    /// Check that `DepositScript::deposit_script` and the
    /// `parse_deposit_script` function are inverses of one another.
    #[test_case(PrincipalData::from(StacksAddress::burn_address(false)) ; "standard address")]
    #[test_case(PrincipalData::parse(CONTRACT_ADDRESS).unwrap(); "contract address")]
    fn deposit_script_parsing_and_creation_are_inverses(recipient: PrincipalData) {
        let secret_key = SecretKey::new(&mut OsRng);

        let deposit = DepositScriptInputs {
            signers_public_key: secret_key.x_only_public_key(SECP256K1).0,
            max_fee: 15000,
            recipient,
        };

        let deposit_script = deposit.deposit_script();
        let parsed_deposit = DepositScriptInputs::parse(&deposit_script).unwrap();

        assert_eq!(deposit, parsed_deposit);
    }

    #[test]
    fn deposit_script_128_byte_contract_name() {
        let contract_name = std::iter::repeat('a').take(128).collect::<String>();
        let principal_str = format!("{}.{contract_name}", StacksAddress::burn_address(false));
        let secret_key = SecretKey::new(&mut OsRng);

        let deposit = DepositScriptInputs {
            signers_public_key: secret_key.x_only_public_key(SECP256K1).0,
            max_fee: 25000,
            recipient: PrincipalData::parse(&principal_str).unwrap(),
        };

        assert_eq!(deposit.recipient.serialize_to_vec().len(), 151);

        let deposit_script = deposit.deposit_script();
        let parsed_deposit = DepositScriptInputs::parse(&deposit_script).unwrap();

        assert_eq!(deposit, parsed_deposit);
    }

    /// Basic check that we can create an address without any issues
    #[test_case(PrincipalData::from(StacksAddress::burn_address(false)) ; "standard address")]
    #[test_case(PrincipalData::parse(CONTRACT_ADDRESS).unwrap(); "contract address")]
    fn btc_address(recipient: PrincipalData) {
        let secret_key = SecretKey::new(&mut OsRng);

        let deposit = DepositScriptInputs {
            signers_public_key: secret_key.x_only_public_key(SECP256K1).0,
            max_fee: 15000,
            recipient,
        };

        let address = deposit.to_address(ScriptBuf::new(), Network::Regtest);
        assert_eq!(address.address_type(), Some(AddressType::P2tr));
    }

    #[test_case(0; "sneaky guy setting the lock time to zero")]
    #[test_case(6; "6, a minimal number")]
    #[test_case(15; "15, another minimal number")]
    #[test_case(0x00000000ff; "1 byte non-minimal")]
    #[test_case(0x000000ffff; "2 bytes non-minimal")]
    #[test_case(0x0000ffffff; "3 bytes non-minimal")]
    #[test_case(0x005f000000; "4 bytes non-minimal")]
    #[test_case(0x7fffffffff; "5 bytes non-minimal 2**39 - 1")]
    fn reclaim_script_lock_time(lock_time: i64) {
        let reclaim_script = reclaim_p2pk(lock_time);

        let extracts = ReclaimScriptInputs::parse(&reclaim_script).unwrap();
        assert_eq!(extracts.lock_time, lock_time);
        assert_eq!(extracts.reclaim_script(), reclaim_script);

        // Let's check that ReclaimScriptInputs::reclaim_script and
        // parse_reclaim_script and are inverses in the other direction.
        let script = ScriptBuf::builder()
            .push_opcode(opcodes::OP_DROP)
            .push_slice([2; 32])
            .push_opcode(opcodes::OP_CHECKSIG)
            .into_script();

        let inputs = ReclaimScriptInputs::try_new(lock_time, script).unwrap();
        let reclaim_script = inputs.reclaim_script();
        assert_eq!(ReclaimScriptInputs::parse(&reclaim_script).unwrap(), inputs);
    }

    #[test]
    fn reclaim_6_bytes_bad() {
        let lock_time = 0x10000000000;
        let reclaim_script = reclaim_p2pk(lock_time);
        assert!(ReclaimScriptInputs::parse(&reclaim_script).is_err());
    }

    #[test_case(-1; "negative one")]
    #[test_case(-16; "negative sixteen")]
    #[test_case(-1000; "negative 1000")]
    fn reclaim_negative_numbers_bytes_bad(lock_time: i64) {
        let reclaim_script = reclaim_p2pk(lock_time);

        match ReclaimScriptInputs::parse(&reclaim_script) {
            Err(Error::InvalidReclaimScriptLockTime(parsed_lock_time)) => {
                assert_eq!(parsed_lock_time, lock_time)
            }
            _ => panic!("This shouldn't trigger"),
        };
    }

    #[test]
    fn no_real_reclaim_script_is_fine() {
        let lock_time = 150;
        let reclaim_script = ScriptBuf::builder()
            .push_int(lock_time)
            .push_opcode(opcodes::OP_CSV)
            .into_script();

        let extracts = ReclaimScriptInputs::parse(&reclaim_script).unwrap();
        assert_eq!(extracts.lock_time, lock_time);
        assert_eq!(extracts.reclaim_script(), reclaim_script);
    }

    #[test_case(ScriptBuf::builder()
        .push_opcode(opcodes::OP_RETURN)
        .push_int(150)
        .push_opcode(opcodes::OP_CSV)
        .into_script() ; "start with OP_RETURN")]
    #[test_case(ScriptBuf::builder()
        .push_opcode(opcodes::OP_PUSHNUM_1)
        .push_int(150)
        .push_opcode(opcodes::OP_CSV)
        .into_script() ; "start with OP_TRUE")]
    #[test_case(ScriptBuf::builder()
        .push_int(0)
        .push_int(150)
        .push_opcode(opcodes::OP_CSV)
        .into_script() ; "start with 0")]
    fn invalid_script_start(reclaim_script: ScriptBuf) {
        // The script must start with `<lock-time> OP_CSV` or we get an
        // error.
        assert!(ReclaimScriptInputs::parse(&reclaim_script).is_err());
    }

    #[test]
    fn happy_path_tx_validation() {
        let max_fee: u64 = 15000;
        let amount_sats = 500_000;
        let lock_time = 150;

        let setup: TxSetup = tx_setup(lock_time, max_fee, amount_sats);

        let tx_hex = bitcoin::consensus::encode::serialize_hex(&setup.tx);
        let request = CreateDepositRequest {
            outpoint: OutPoint::new(setup.tx.compute_txid(), 0),
            reclaim_script: setup.reclaim.reclaim_script(),
            deposit_script: setup.deposit.deposit_script(),
        };

        let parsed = request.validate_tx(&tx_hex).unwrap();

        assert_eq!(parsed.outpoint, request.outpoint);
        assert_eq!(parsed.deposit_script, request.deposit_script);
        assert_eq!(parsed.reclaim_script, request.reclaim_script);
        assert_eq!(parsed.amount, amount_sats);
        assert_eq!(parsed.signers_public_key, setup.deposit.signers_public_key);
        assert_eq!(parsed.lock_time, lock_time as u64);
        assert_eq!(parsed.recipient, setup.deposit.recipient);
    }

    #[test]
    fn valid_deposit_script_not_matching_tx_rejected() {
        let max_fee: u64 = 15000;
        let amount_sats = 500_000;
        let lock_time = 150;

        let mut setup: TxSetup = tx_setup(lock_time, max_fee, amount_sats);

        let tx_hex = bitcoin::consensus::encode::serialize_hex(&setup.tx);

        // Let's modify the max_fee of the deposit script and send that in
        // the request.
        setup.deposit.max_fee = 3000;

        let request = CreateDepositRequest {
            outpoint: OutPoint::new(setup.tx.compute_txid(), 0),
            deposit_script: setup.deposit.deposit_script(),
            reclaim_script: setup.reclaim.reclaim_script(),
        };

        let error = request.validate_tx(&tx_hex).unwrap_err();
        assert!(matches!(error, Error::UtxoScriptPubKeyMismatch(_)));
    }

    #[test]
    fn valid_reclaim_script_not_matching_tx_rejected() {
        let max_fee: u64 = 15000;
        let amount_sats = 500_000;
        let lock_time = 0;

        let mut setup: TxSetup = tx_setup(lock_time, max_fee, amount_sats);

        let tx_hex = bitcoin::consensus::encode::serialize_hex(&setup.tx);

        // Let's modify the lock time of the reclaim script to look more
        // reasonable in the request.
        setup.reclaim.lock_time = 150;

        let request = CreateDepositRequest {
            outpoint: OutPoint::new(setup.tx.compute_txid(), 0),
            deposit_script: setup.deposit.deposit_script(),
            reclaim_script: setup.reclaim.reclaim_script(),
        };

        let error = request.validate_tx(&tx_hex).unwrap_err();
        assert!(matches!(error, Error::UtxoScriptPubKeyMismatch(_)));
    }

    #[test]
    fn incorrect_tx_outpoint_rejected() {
        let max_fee: u64 = 15000;
        let amount_sats = 500_000;
        let lock_time = 150;

        let setup: TxSetup = tx_setup(lock_time, max_fee, amount_sats);

        let tx_hex = bitcoin::consensus::encode::serialize_hex(&setup.tx);

        let request = CreateDepositRequest {
            // This output index is guaranteed to always be incorrect.
            outpoint: OutPoint::new(setup.tx.compute_txid(), setup.tx.output.len() as u32),
            deposit_script: setup.deposit.deposit_script(),
            reclaim_script: setup.reclaim.reclaim_script(),
        };

        let error = request.validate_tx(&tx_hex).unwrap_err();
        assert!(matches!(error, Error::OutpointIndex(_, _)));

        let request = CreateDepositRequest {
            // This txid is almost certainly incorrect.
            outpoint: OutPoint::new(Txid::all_zeros(), 0),
            deposit_script: setup.deposit.deposit_script(),
            reclaim_script: setup.reclaim.reclaim_script(),
        };

        let error = request.validate_tx(&tx_hex).unwrap_err();
        assert!(matches!(error, Error::TxidMismatch { .. }));
    }

    #[test]
    fn incorrect_tx_hex_rejected() {
        let max_fee: u64 = 15000;
        let amount_sats = 500_000;
        let lock_time = 150;

        let setup: TxSetup = tx_setup(lock_time, max_fee, amount_sats);

        let request = CreateDepositRequest {
            // This output index is guaranteed to be incorrect.
            outpoint: OutPoint::new(setup.tx.compute_txid(), 0),
            deposit_script: setup.deposit.deposit_script(),
            reclaim_script: setup.reclaim.reclaim_script(),
        };

        let error = request.validate_tx("abc123").unwrap_err();
        assert!(matches!(error, Error::DecodeFromHex(_)));
    }

    #[test]
    fn correct_tx_request_has_invalid_deposit_or_reclaim_script() {
        let max_fee: u64 = 15000;
        let amount_sats = 500_000;
        let lock_time = 150;

        let setup: TxSetup = tx_setup(lock_time, max_fee, amount_sats);

        let tx_hex = bitcoin::consensus::encode::serialize_hex(&setup.tx);

        let request = CreateDepositRequest {
            outpoint: OutPoint::new(setup.tx.compute_txid(), 0),
            // The actual deposit script in the transaction is fine, but
            // they told us a lie and sent us an invalid deposit script in
            // their request.
            deposit_script: ScriptBuf::new(),
            reclaim_script: setup.reclaim.reclaim_script(),
        };

        let error = request.validate_tx(&tx_hex).unwrap_err();
        assert!(matches!(error, Error::InvalidDepositScript));

        let request = CreateDepositRequest {
            outpoint: OutPoint::new(setup.tx.compute_txid(), 0),
            deposit_script: setup.deposit.deposit_script(),
            // The actual reclaim script in the transaction is fine, but
            // they told us a lie, and sent us an invalid reclaim script in
            // their request.
            reclaim_script: ScriptBuf::new(),
        };

        let error = request.validate_tx(&tx_hex).unwrap_err();
        assert!(matches!(error, Error::InvalidReclaimScript));
    }
}