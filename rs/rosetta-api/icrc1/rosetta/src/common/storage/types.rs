use candid::Deserialize;
use ic_icrc1::blocks::{generic_block_to_encoded_block, generic_transaction_from_generic_block};
use ic_icrc1::{Block, Transaction};
use ic_icrc1_tokens_u64::U64;
use ic_ledger_canister_core::ledger::LedgerTransaction;
use ic_ledger_core::block::{BlockType, EncodedBlock};
use icrc_ledger_types::icrc::generic_metadata_value::MetadataValue;
use icrc_ledger_types::icrc3::blocks::GenericBlock;
use rosetta_core::identifiers::BlockIdentifier;
use serde::Serialize;
use serde_bytes::ByteBuf;

type Tokens = U64;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RosettaBlock {
    pub index: u64,
    pub parent_hash: Option<ByteBuf>,
    pub block_hash: ByteBuf,
    pub encoded_block: EncodedBlock,
    pub transaction_hash: ByteBuf,
    pub timestamp: u64,
}

impl RosettaBlock {
    pub fn from_generic_block(generic_block: GenericBlock, block_idx: u64) -> anyhow::Result<Self> {
        let block_hash = ByteBuf::from(generic_block.hash());
        let block =
            generic_block_to_encoded_block(generic_block.clone()).map_err(anyhow::Error::msg)?;
        let block = Block::<Tokens>::decode(block).map_err(anyhow::Error::msg)?;
        let transaction_hash = ByteBuf::from(
            generic_transaction_from_generic_block(generic_block)
                .map_err(anyhow::Error::msg)?
                .hash(),
        );
        let timestamp = block.timestamp;

        Ok(Self {
            index: block_idx,
            parent_hash: Block::parent_hash(&block).map(|eb| ByteBuf::from(eb.as_slice().to_vec())),
            block_hash,
            timestamp,
            encoded_block: block.encode(),
            transaction_hash,
        })
    }
    pub fn from_icrc_ledger_block(block: Block<Tokens>, block_idx: u64) -> anyhow::Result<Self> {
        let eb = block.clone().encode();
        Ok(Self {
            index: block_idx,
            parent_hash: Block::parent_hash(&block).map(|eb| ByteBuf::from(eb.as_slice().to_vec())),
            block_hash: ByteBuf::from(
                <Block<Tokens> as BlockType>::block_hash(&eb)
                    .as_slice()
                    .to_vec(),
            ),
            encoded_block: eb,
            transaction_hash: ByteBuf::from(
                <Transaction<Tokens> as LedgerTransaction>::hash(&block.transaction)
                    .as_slice()
                    .to_vec(),
            ),
            timestamp: block.timestamp,
        })
    }

    pub fn from_encoded_block(eb: EncodedBlock, block_idx: u64) -> anyhow::Result<Self> {
        RosettaBlock::from_icrc_ledger_block(
            Block::decode(eb).map_err(anyhow::Error::msg)?,
            block_idx,
        )
    }

    pub fn get_effective_fee(&self) -> anyhow::Result<Option<Tokens>> {
        Block::decode(self.encoded_block.clone())
            .map(|b| b.effective_fee)
            .map_err(anyhow::Error::msg)
    }

    pub fn get_transaction(&self) -> anyhow::Result<Transaction<Tokens>> {
        Ok(Block::decode(self.encoded_block.clone())
            .map_err(anyhow::Error::msg)?
            .transaction)
    }
}

impl From<&RosettaBlock> for BlockIdentifier {
    fn from(block: &RosettaBlock) -> Self {
        Self {
            index: block.index,
            hash: hex::encode(&block.block_hash),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MetadataEntry {
    pub key: String,
    pub value: Vec<u8>,
}

impl MetadataEntry {
    pub fn from_metadata_value(key: &str, value: &MetadataValue) -> anyhow::Result<Self> {
        let value = candid::encode_one(value)?;

        Ok(Self {
            key: key.to_string(),
            value,
        })
    }

    pub fn value(&self) -> anyhow::Result<MetadataValue> {
        Ok(candid::decode_one(&self.value)?)
    }
}
