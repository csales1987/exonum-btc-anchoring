use std::fmt;
use std::collections::HashMap;
use std::ops::Deref;

use byteorder::{ByteOrder, LittleEndian};
use bitcoin::blockdata::script::Instruction;
use bitcoin::blockdata::opcodes::All;
use bitcoin::util::hash::Hash160;
use bitcoin::network::serialize::{BitcoinHash, serialize_hex, deserialize, serialize};
use bitcoin::blockdata::transaction::{TxIn, TxOut};
use bitcoin::blockdata::script::{Script, Builder};
use bitcoin::util::base58::ToBase58;
use bitcoin::util::address::{Address, Privkey, Type};
use bitcoin::network::constants::Network;
use secp256k1::key::PublicKey;
use bitcoinrpc;

// FIXME do not use Hash from crypto, use Sha256Hash explicit
use exonum::crypto::{hash, Hash, FromHexError, HexValue};
use exonum::node::Height;
use exonum::storage::StorageValue;

use {AnchoringRpc, RpcClient, HexValueEx, BitcoinSignature, Result};
use multisig::{sign_input, verify_input, RedeemScript};
use btc;
use btc::TxId;

pub type RawBitcoinTx = ::bitcoin::blockdata::transaction::Transaction;

const ANCHORING_TX_FUNDS_OUTPUT: u32 = 0;
const ANCHORING_TX_DATA_OUTPUT: u32 = 1;
// Структура у анкорящей транзакции строгая:
// - нулевой вход это прошлая анкорящая транзакция или фундирующая, если транзакция исходная
// - нулевой выход это всегда следующая анкорящая транзакция
// - первый выход это метаданные
// Итого транзакции у которых нулевой вход нам не известен, а выходов не два или они содержат другую информацию,
// считаются априори не валидными.
#[derive(Clone, PartialEq)]
pub struct AnchoringTx(pub RawBitcoinTx);
// Структура валидной фундирующей транзакции тоже строгая:
// Входов и выходов может быть несколько, но главное правило, чтобы нулевой вход переводил деньги на мультисиг кошелек
#[derive(Clone, PartialEq)]
pub struct FundingTx(pub RawBitcoinTx);
// Обертка над обычной биткоин транзакцией
#[derive(Debug, Clone, PartialEq)]
pub struct BitcoinTx(pub RawBitcoinTx);

#[derive(Debug, Clone, PartialEq)]
pub enum TxKind {
    Anchoring(AnchoringTx),
    FundingTx(FundingTx),
    Other(BitcoinTx),
}

pub struct TransactionBuilder {
    inputs: Vec<(RawBitcoinTx, u32)>,
    output: Option<btc::Address>,
    fee: Option<u64>,
    payload: Option<(u64, Hash)>,
}

impl HexValueEx for RawBitcoinTx {
    fn to_hex(&self) -> String {
        serialize_hex(self).unwrap()
    }
    fn from_hex<T: AsRef<str>>(v: T) -> ::std::result::Result<Self, FromHexError> {
        let bytes = Vec::<u8>::from_hex(v.as_ref())?;
        if let Ok(tx) = deserialize(bytes.as_ref()) {
            Ok(tx)
        } else {
            Err(FromHexError::InvalidHexLength)
        }
    }
}

implement_tx_wrapper! {AnchoringTx}
implement_tx_wrapper! {FundingTx}
implement_tx_wrapper! {BitcoinTx}

implement_tx_from_raw! {AnchoringTx}
implement_tx_from_raw! {FundingTx}

impl FundingTx {
    pub fn create(client: &AnchoringRpc,
                  address: &btc::Address,
                  total_funds: u64)
                  -> Result<FundingTx> {
        let tx = client.send_to_address(address, total_funds)?;
        Ok(FundingTx::from(tx))
    }

    pub fn find_out(&self, addr: &btc::Address) -> Option<u32> {
        let redeem_script_hash = addr.hash;
        self.0
            .output
            .iter()
            .position(|output| if let Some(Instruction::PushBytes(bytes)) =
                output.script_pubkey.into_iter().nth(1) {
                Hash160::from(bytes) == redeem_script_hash
            } else {
                false
            })
            .map(|x| x as u32)
    }

    pub fn is_unspent(&self,
                      client: &RpcClient,
                      addr: &btc::Address)
                      -> Result<Option<bitcoinrpc::UnspentTransactionInfo>> {
        let txid = self.txid();
        let txs = client.listunspent(0, 9999999, [addr.to_base58check().as_ref()])?;
        Ok(txs.into_iter()
            .find(|txinfo| txinfo.txid == txid))
    }
}

impl AnchoringTx {
    pub fn amount(&self) -> u64 {
        self.0.output[ANCHORING_TX_FUNDS_OUTPUT as usize].value
    }

    pub fn output_address(&self, network: Network) -> btc::Address {
        let ref script = self.0.output[ANCHORING_TX_FUNDS_OUTPUT as usize].script_pubkey;
        let bytes = script.into_iter()
            .filter_map(|instruction| if let Instruction::PushBytes(bytes) = instruction {
                Some(bytes)
            } else {
                None
            })
            .next()
            .unwrap();

        Address {
                ty: Type::ScriptHash,
                network: network,
                hash: Hash160::from(bytes),
            }
            .into()
    }

    pub fn inputs(&self) -> ::std::ops::Range<u32> {
        0..self.0.input.len() as u32
    }

    pub fn payload(&self) -> (Height, Hash) {
        find_payload(&self.0).expect("Unable to find payload")
    }

    pub fn prev_hash(&self) -> TxId {
        TxId::from(self.0.input[0].prev_hash)
    }

    pub fn get_info(&self, client: &RpcClient) -> Result<Option<bitcoinrpc::RawTransactionInfo>> {
        let r = client.getrawtransaction_verbose(&self.txid());
        match r {
            Ok(tx) => Ok(Some(tx)),
            Err(bitcoinrpc::Error::NoInformation(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn sign(&self,
                redeem_script: &btc::RedeemScript,
                input: u32,
                priv_key: &Privkey)
                -> BitcoinSignature {
        sign_anchoring_transaction(self, redeem_script, input, priv_key)
    }

    pub fn verify(&self,
                  redeem_script: &RedeemScript,
                  input: u32,
                  pub_key: &PublicKey,
                  signature: &[u8])
                  -> bool {
        verify_anchoring_transaction(self, redeem_script, input, pub_key, signature)
    }

    pub fn finalize(self,
                    redeem_script: &btc::RedeemScript,
                    signatures: HashMap<u32, Vec<BitcoinSignature>>)
                    -> Result<AnchoringTx> {
        let tx = finalize_anchoring_transaction(self, redeem_script, signatures);
        Ok(tx)
    }

    pub fn send(self,
                client: &AnchoringRpc,
                redeem_script: &btc::RedeemScript,
                signatures: HashMap<u32, Vec<BitcoinSignature>>)
                -> Result<AnchoringTx> {
        let tx = self.finalize(redeem_script, signatures)?;
        client.send_transaction(tx.clone().into())?;
        Ok(tx)
    }
}

impl fmt::Debug for AnchoringTx {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let payload = self.payload();
        f.debug_struct(stringify!(AnchoringTx))
            .field("txid", &self.txid())
            .field("txhex", &self.to_hex())
            .field("content", &self.0)
            .field("height", &payload.0)
            .field("hash", &payload.1.to_hex())
            .finish()
    }
}

impl fmt::Debug for FundingTx {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct(stringify!(AnchoringTx))
            .field("txid", &self.txid())
            .field("txhex", &self.to_hex())
            .field("content", &self.0)
            .finish()
    }
}

impl TxKind {
    pub fn from_txid(client: &AnchoringRpc, txid: Hash) -> Result<TxKind> {
        let tx = client.get_transaction(txid.to_hex().as_ref())?;
        Ok(TxKind::from(tx))
    }
}

impl From<RawBitcoinTx> for TxKind {
    fn from(tx: RawBitcoinTx) -> TxKind {
        if find_payload(&tx).is_some() {
            TxKind::Anchoring(AnchoringTx::from(tx))
        } else {
            // TODO make sure that only first output[0] is p2sh
            // Find output with funds and p2sh script_pubkey
            for out in tx.output.iter() {
                if out.value > 0 && out.script_pubkey.is_p2sh() {
                    return TxKind::FundingTx(FundingTx::from(tx.clone()));
                }
            }
            TxKind::Other(BitcoinTx::from(tx))
        }
    }
}

impl From<BitcoinTx> for TxKind {
    fn from(tx: BitcoinTx) -> TxKind {
        TxKind::from(tx.0)
    }
}

impl TransactionBuilder {
    pub fn with_prev_tx(prev_tx: &RawBitcoinTx, out: u32) -> TransactionBuilder {
        TransactionBuilder {
            inputs: vec![(prev_tx.clone(), out)],
            output: None,
            payload: None,
            fee: None,
        }
    }

    pub fn fee(mut self, fee: u64) -> TransactionBuilder {
        self.fee = Some(fee);
        self
    }

    pub fn add_funds(mut self, tx: &RawBitcoinTx, out: u32) -> TransactionBuilder {
        self.inputs.push((tx.clone(), out));
        self
    }

    pub fn payload(mut self, height: u64, hash: Hash) -> TransactionBuilder {
        self.payload = Some((height, hash));
        self
    }

    pub fn send_to(mut self, addr: btc::Address) -> TransactionBuilder {
        self.output = Some(addr);
        self
    }

    pub fn into_transaction(mut self) -> AnchoringTx {
        let total_funds: u64 = self.inputs
            .iter()
            .map(|&(ref tx, out)| tx.output[out as usize].value)
            .sum();

        let addr = self.output.take().expect("Output address is not set");
        let fee = self.fee.expect("Fee is not set");
        let (height, block_hash) = self.payload.take().unwrap();
        create_anchoring_transaction(addr,
                                     height,
                                     block_hash,
                                     self.inputs.iter(),
                                     total_funds - fee)
    }
}

fn create_anchoring_transaction<'a, I>(addr: btc::Address,
                                       block_height: Height,
                                       block_hash: Hash,
                                       inputs: I,
                                       out_funds: u64)
                                       -> AnchoringTx
    where I: Iterator<Item = &'a (RawBitcoinTx, u32)>
{
    let inputs = inputs.map(|&(ref unspent_tx, utxo_vout)| {
            TxIn {
                prev_hash: unspent_tx.bitcoin_hash(),
                prev_index: utxo_vout,
                script_sig: Script::new(),
                sequence: 0xFFFFFFFF,
            }
        })
        .collect::<Vec<_>>();

    let metadata_script = {
        let data = {
            let mut data = [0u8; 42];
            data[0] = 1; // version
            data[1] = 40; // data len
            LittleEndian::write_u64(&mut data[2..10], block_height);
            data[10..42].copy_from_slice(block_hash.as_ref());
            data
        };
        Builder::new()
            .push_opcode(All::OP_RETURN)
            .push_slice(data.as_ref())
            .into_script()
    };
    let outputs = vec![TxOut {
                           value: out_funds,
                           script_pubkey: addr.script_pubkey(),
                       },
                       TxOut {
                           value: 0,
                           script_pubkey: metadata_script,
                       }];

    let tx = RawBitcoinTx {
        version: 1,
        lock_time: 0,
        input: inputs,
        output: outputs,
        witness: vec![],
    };
    AnchoringTx::from(tx)
}

fn sign_anchoring_transaction(tx: &RawBitcoinTx,
                              redeem_script: &btc::RedeemScript,
                              vin: u32,
                              priv_key: &Privkey)
                              -> BitcoinSignature {
    let signature = sign_input(tx, vin as usize, &redeem_script, priv_key.secret_key());
    signature
}

fn verify_anchoring_transaction(tx: &RawBitcoinTx,
                                redeem_script: &RedeemScript,
                                vin: u32,
                                pub_key: &PublicKey,
                                signature: &[u8])
                                -> bool {
    verify_input(tx, vin as usize, redeem_script, pub_key, signature)
}

fn finalize_anchoring_transaction(mut anchoring_tx: AnchoringTx,
                                  redeem_script: &btc::RedeemScript,
                                  signatures: HashMap<u32, Vec<BitcoinSignature>>)
                                  -> AnchoringTx {
    let redeem_script_bytes = redeem_script.0.clone().into_vec();
    // build scriptSig
    for (out, signatures) in signatures.into_iter() {
        anchoring_tx.0.input[out as usize].script_sig = {
            let mut builder = Builder::new();
            builder = builder.push_opcode(All::OP_PUSHBYTES_0);
            for sign in &signatures {
                builder = builder.push_slice(sign.as_ref());
            }
            builder.push_slice(redeem_script_bytes.as_ref())
                .into_script()
        };
    }
    anchoring_tx
}

fn find_payload(tx: &RawBitcoinTx) -> Option<(Height, Hash)> {
    tx.output
        .get(ANCHORING_TX_DATA_OUTPUT as usize)
        .and_then(|output| {
            output.script_pubkey
                .into_iter()
                .filter_map(|instruction| if let Instruction::PushBytes(bytes) = instruction {
                    Some(bytes)
                } else {
                    None
                })
                .next()
        })
        .and_then(|bytes| if bytes.len() == 42 && bytes[0] == 1 {
            // TODO check len
            let height = LittleEndian::read_u64(&bytes[2..10]);
            let block_hash = Hash::from_slice(&bytes[10..42]).unwrap();
            Some((height, block_hash))
        } else {
            None
        })
}

#[cfg(test)]
mod tests {
    extern crate blockchain_explorer;

    use std::collections::HashMap;

    use bitcoin::network::constants::Network;
    use bitcoin::util::base58::{FromBase58, ToBase58};

    use exonum::crypto::{Hash, HexValue};

    use multisig::RedeemScript;
    use transactions::{BitcoinTx, AnchoringTx, FundingTx, TransactionBuilder, TxKind};
    use btc;

    #[test]
    fn test_anchoring_tx_sign() {
        let _ = blockchain_explorer::helpers::init_logger();

        let priv_keys = ["cVC9eJN5peJemWn1byyWcWDevg6xLNXtACjHJWmrR5ynsCu8mkQE",
                         "cMk66oMazTgquBVaBLHzDi8FMgAaRN3tSf6iZykf9bCh3D3FsLX1",
                         "cT2S5KgUQJ41G6RnakJ2XcofvoxK68L9B44hfFTnH4ddygaxi7rc",
                         "cRUKB8Nrhxwd5Rh6rcX3QK1h7FosYPw5uzEsuPpzLcDNErZCzSaj"]
            .iter()
            .map(|x| btc::PrivateKey::from_base58check(x).unwrap())
            .collect::<Vec<_>>();

        let pub_keys = ["03475ab0e9cfc6015927e662f6f8f088de12287cee1a3237aeb497d1763064690c",
                        "02a63948315dda66506faf4fecd54b085c08b13932a210fa5806e3691c69819aa0",
                        "0230cb2805476bf984d2236b56ff5da548dfe116daf2982608d898d9ecb3dceb49",
                        "036e4777c8d19ccaa67334491e777f221d37fd85d5786a4e5214b281cf0133d65e"]
            .iter()
            .map(|x| btc::PublicKey::from_hex(x).unwrap())
            .collect::<Vec<_>>();
        let redeem_script = RedeemScript::from_pubkeys(pub_keys.iter(), 3)
            .compressed(Network::Testnet);

        let prev_tx = AnchoringTx::from_hex("01000000014970bd8d76edf52886f62e3073714bddc6c33bccebb6b1d06db8c87fb1103ba000000000fd670100483045022100e6ef3de83437c8dc33a8099394b7434dfb40c73631fc4b0378bd6fb98d8f42b002205635b265f2bfaa6efc5553a2b9e98c2eabdfad8e8de6cdb5d0d74e37f1e198520147304402203bb845566633b726e41322743677694c42b37a1a9953c5b0b44864d9b9205ca10220651b7012719871c36d0f89538304d3f358da12b02dab2b4d74f2981c8177b69601473044022052ad0d6c56aa6e971708f079073260856481aeee6a48b231bc07f43d6b02c77002203a957608e4fbb42b239dd99db4e243776cc55ed8644af21fa80fd9be77a59a60014c8b532103475ab0e9cfc6015927e662f6f8f088de12287cee1a3237aeb497d1763064690c2102a63948315dda66506faf4fecd54b085c08b13932a210fa5806e3691c69819aa0210230cb2805476bf984d2236b56ff5da548dfe116daf2982608d898d9ecb3dceb4921036e4777c8d19ccaa67334491e777f221d37fd85d5786a4e5214b281cf0133d65e54aeffffffff02b80b00000000000017a914bff50e89fa259d83f78f2e796f57283ca10d6e678700000000000000002c6a2a01280000000000000000f1cb806d27e367f1cac835c22c8cc24c402a019e2d3ea82f7f841c308d830a9600000000").unwrap();
        let funding_tx = FundingTx::from_hex("01000000019532a4022a22226a6f694c3f21216b2c9f5c1c79007eb7d3be06bc2f1f9e52fb000000006a47304402203661efd05ca422fad958b534dbad2e1c7db42bbd1e73e9b91f43a2f7be2f92040220740cf883273978358f25ca5dd5700cce5e65f4f0a0be2e1a1e19a8f168095400012102ae1b03b0f596be41a247080437a50f4d8e825b170770dcb4e5443a2eb2ecab2afeffffff02a00f00000000000017a914bff50e89fa259d83f78f2e796f57283ca10d6e678716e1ff05000000001976a91402f5d7475a10a9c24cea32575bd8993d3fabbfd388ac089e1000").unwrap();

        let tx = TransactionBuilder::with_prev_tx(&prev_tx, 0)
            .add_funds(&funding_tx, 0)
            .payload(10, Hash::from_hex("164d236bbdb766e64cec57847e3a0509d4fc77fa9c17b7e61e48f7a3eaa8dbc9").unwrap())
            .fee(1000)
            .send_to(btc::Address::from_script(&redeem_script, Network::Testnet))
            .into_transaction();

        let mut signatures = HashMap::new();
        for input in tx.inputs() {
            let mut input_signs = Vec::new();
            for priv_key in priv_keys.iter() {
                let sign = tx.sign(&redeem_script, input, priv_key);
                input_signs.push(sign);
            }
            signatures.insert(input, input_signs);
        }

        for (input, signs) in signatures.iter() {
            for (id, signature) in signs.iter().enumerate() {
                assert!(tx.verify(&redeem_script, *input, &pub_keys[id], signature.as_ref()));
            }
        }
    }

    #[test]
    fn test_anchoring_tx_output_address() {
        let tx = AnchoringTx::from_hex("01000000014970bd8d76edf52886f62e3073714bddc6c33bccebb6b1d06db8c87fb1103ba000000000fd670100483045022100e6ef3de83437c8dc33a8099394b7434dfb40c73631fc4b0378bd6fb98d8f42b002205635b265f2bfaa6efc5553a2b9e98c2eabdfad8e8de6cdb5d0d74e37f1e198520147304402203bb845566633b726e41322743677694c42b37a1a9953c5b0b44864d9b9205ca10220651b7012719871c36d0f89538304d3f358da12b02dab2b4d74f2981c8177b69601473044022052ad0d6c56aa6e971708f079073260856481aeee6a48b231bc07f43d6b02c77002203a957608e4fbb42b239dd99db4e243776cc55ed8644af21fa80fd9be77a59a60014c8b532103475ab0e9cfc6015927e662f6f8f088de12287cee1a3237aeb497d1763064690c2102a63948315dda66506faf4fecd54b085c08b13932a210fa5806e3691c69819aa0210230cb2805476bf984d2236b56ff5da548dfe116daf2982608d898d9ecb3dceb4921036e4777c8d19ccaa67334491e777f221d37fd85d5786a4e5214b281cf0133d65e54aeffffffff02b80b00000000000017a914bff50e89fa259d83f78f2e796f57283ca10d6e678700000000000000002c6a2a01280000000000000000f1cb806d27e367f1cac835c22c8cc24c402a019e2d3ea82f7f841c308d830a9600000000").unwrap();

        let pub_keys = ["03475ab0e9cfc6015927e662f6f8f088de12287cee1a3237aeb497d1763064690c",
                        "02a63948315dda66506faf4fecd54b085c08b13932a210fa5806e3691c69819aa0",
                        "0230cb2805476bf984d2236b56ff5da548dfe116daf2982608d898d9ecb3dceb49",
                        "036e4777c8d19ccaa67334491e777f221d37fd85d5786a4e5214b281cf0133d65e"]
            .iter()
            .map(|x| btc::PublicKey::from_hex(x).unwrap())
            .collect::<Vec<_>>();
        let redeem_script = RedeemScript::from_pubkeys(&pub_keys, 3).compressed(Network::Testnet);

        assert_eq!(tx.output_address(Network::Testnet).to_base58check(),
                   redeem_script.to_address(Network::Testnet));
    }

    #[test]
    fn test_tx_kind_funding() {
        let tx = BitcoinTx::from_hex("01000000019532a4022a22226a6f694c3f21216b2c9f5c1c79007eb7d3be06bc2f1f9e52fb000000006a47304402203661efd05ca422fad958b534dbad2e1c7db42bbd1e73e9b91f43a2f7be2f92040220740cf883273978358f25ca5dd5700cce5e65f4f0a0be2e1a1e19a8f168095400012102ae1b03b0f596be41a247080437a50f4d8e825b170770dcb4e5443a2eb2ecab2afeffffff02a00f00000000000017a914bff50e89fa259d83f78f2e796f57283ca10d6e678716e1ff05000000001976a91402f5d7475a10a9c24cea32575bd8993d3fabbfd388ac089e1000").unwrap();
        match TxKind::from(tx) {
            TxKind::FundingTx(_) => {}
            _ => panic!("Wrong tx kind!"),
        }
    }

    #[test]
    fn test_tx_kind_anchoring() {
        let tx = BitcoinTx::from_hex("01000000014970bd8d76edf52886f62e3073714bddc6c33bccebb6b1d06db8c87fb1103ba000000000fd670100483045022100e6ef3de83437c8dc33a8099394b7434dfb40c73631fc4b0378bd6fb98d8f42b002205635b265f2bfaa6efc5553a2b9e98c2eabdfad8e8de6cdb5d0d74e37f1e198520147304402203bb845566633b726e41322743677694c42b37a1a9953c5b0b44864d9b9205ca10220651b7012719871c36d0f89538304d3f358da12b02dab2b4d74f2981c8177b69601473044022052ad0d6c56aa6e971708f079073260856481aeee6a48b231bc07f43d6b02c77002203a957608e4fbb42b239dd99db4e243776cc55ed8644af21fa80fd9be77a59a60014c8b532103475ab0e9cfc6015927e662f6f8f088de12287cee1a3237aeb497d1763064690c2102a63948315dda66506faf4fecd54b085c08b13932a210fa5806e3691c69819aa0210230cb2805476bf984d2236b56ff5da548dfe116daf2982608d898d9ecb3dceb4921036e4777c8d19ccaa67334491e777f221d37fd85d5786a4e5214b281cf0133d65e54aeffffffff02b80b00000000000017a914bff50e89fa259d83f78f2e796f57283ca10d6e678700000000000000002c6a2a01280000000000000000f1cb806d27e367f1cac835c22c8cc24c402a019e2d3ea82f7f841c308d830a9600000000").unwrap();
        match TxKind::from(tx) {
            TxKind::Anchoring(_) => {}
            _ => panic!("Wrong tx kind!"),
        }
    }

    #[test]
    fn test_tx_kind_other() {
        let tx = BitcoinTx::from_hex("0100000001cea827387bc0bb1b5e6afa6e6d557123e4432e47bad8c2d94214a9cd1e2e074b010000006a473044022034d463312dd75445ad078b1159a75c0b148388b36686b69da8aecca863e63dc3022071ef86a064bd15f11ec89059072bbd3e3d3bb6c5e9b10712e0e2dc6710520bb00121035e63a48d34250dbbcc58fdc0ab63b901769e71035e19e0eee1a87d433a96723afeffffff0296a6f80b000000001976a914b5d7055cfdacc803e5547b981faa693c5aaa813b88aca0860100000000001976a914f5548cb02bb197f071934a0ea3eeb5878cb59dff88ac03a21000").unwrap();
        match TxKind::from(tx) {
            TxKind::Other(_) => {}
            _ => panic!("Wrong tx kind!"),
        }
    }
}
