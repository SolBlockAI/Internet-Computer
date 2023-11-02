use crate::common::storage::{storage_client::StorageClient, types::RosettaBlock};
use candid::{Decode, Encode, Nat};
use ic_crypto_tree_hash::{LookupStatus, MixedHashTree};
use ic_icrc1::hash::Hash;
use icrc_ledger_agent::Icrc1Agent;
use icrc_ledger_types::icrc3::blocks::{BlockRange, GetBlocksRequest, GetBlocksResponse};
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use num_traits::ToPrimitive;
use serde_bytes::ByteBuf;
use std::{cmp, collections::HashMap, fmt::Write, ops::RangeInclusive, sync::Arc};
use LookupStatus::Found;

// The Range of indices to be synchronized.
// Contains the hashes of the top and end of the index range, which is used to ensure the fetched block interval is valid.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SyncRange {
    index_range: RangeInclusive<u64>,
    leading_block_hash: ByteBuf,
    trailing_parent_hash: Option<ByteBuf>,
}

impl SyncRange {
    fn new(
        lowest_index: u64,
        highest_index: u64,
        leading_block_hash: ByteBuf,
        trailing_parent_hash: Option<ByteBuf>,
    ) -> Self {
        Self {
            index_range: RangeInclusive::new(lowest_index, highest_index),
            leading_block_hash,
            trailing_parent_hash,
        }
    }
}

/// This function will check whether there is a gap in the database.
/// Furthermore, if there exists a gap between the genesis block and the lowest stored block, this function will add this synchronization gap to the gaps returned by the storage client.
/// It is guaranteed that all gaps between [0,Highest_Stored_Block] will be returned.
fn derive_synchronization_gaps(
    storage_client: Arc<StorageClient>,
) -> anyhow::Result<Vec<SyncRange>> {
    let lowest_block_opt = storage_client.get_block_with_lowest_block_idx()?;

    // If the database is empty then there cannot exist any gaps.
    if lowest_block_opt.is_none() {
        return Ok(vec![]);
    }

    // Unwrap is safe.
    let lowest_block = lowest_block_opt.unwrap();

    // If the database is not empty we have to determine whether there is a gap in the database.
    let gap = storage_client.get_blockchain_gaps()?;

    // The database should have at most one gap. Otherwise the database file was edited and it can no longer be guaranteed that it contains valid blocks.
    if gap.len() > 1 {
        return Err(anyhow::Error::msg(format!("The database has {} gaps. More than one gap means the database has been tampered with and can no longer be guaranteed to contain valid blocks",gap.len())));
    }

    let mut sync_ranges = gap
        .into_iter()
        .map(|(a, b)| {
            SyncRange::new(
                a.index + 1,
                b.index - 1,
                b.parent_hash.unwrap(),
                Some(a.block_hash),
            )
        })
        .collect::<Vec<SyncRange>>();

    // Gaps are only determined within stored block ranges. Blocks with indices that are below the lowest stored block and above the highest stored blocks are not considered.
    // Check if the lowest block that was stored is the genesis block.
    if lowest_block.index != 0 {
        // If the lowest stored block's index is not 0 that means there is a gap between the genesis block and the lowest stored block. Unwrapping parent hash is safe as only the genesis block does not have a parent hash.
        // The first interval to sync is between the genesis block and the lowest stored block.
        sync_ranges.insert(
            0,
            SyncRange::new(
                0,
                lowest_block.index - 1,
                lowest_block.parent_hash.unwrap(),
                None,
            ),
        );
    }
    Ok(sync_ranges)
}

/// This function will check for any gaps in the database and between the database and the icrc ledger
/// After this function is successfully executed all blocks between [0,Ledger_Tip] will be stored in the database.
pub async fn start_synching_blocks(
    agent: Arc<Icrc1Agent>,
    storage_client: Arc<StorageClient>,
    maximum_blocks_per_request: u64,
) -> anyhow::Result<()> {
    // Determine whether there are any synchronization gaps in the database that need to be filled.
    let sync_gaps = derive_synchronization_gaps(storage_client.clone())?;

    // Close all of the synchronization gaps.
    for gap in sync_gaps {
        sync_blocks_interval(
            agent.clone(),
            storage_client.clone(),
            maximum_blocks_per_request,
            gap,
        )
        .await?;
    }

    // After all the gaps have been filled continue with a synchronization from the top of the blockchain.
    sync_from_the_tip(agent, storage_client, maximum_blocks_per_request).await?;

    Ok(())
}

/// This function will do a synchronization of the interval (Highest_Stored_Block,Ledger_Tip].
pub async fn sync_from_the_tip(
    agent: Arc<Icrc1Agent>,
    storage_client: Arc<StorageClient>,
    maximum_blocks_per_request: u64,
) -> anyhow::Result<()> {
    let (tip_block_index, tip_block_hash) = fetch_blockchain_tip_data(agent.clone()).await?;

    // The starting point of the synchronization process is either 0 if the database is empty or the highest stored block index plus one.
    // The trailing parent hash is either `None` if the database is empty or the block hash of the block with the highest block index in storage.
    let sync_range = storage_client.get_block_with_highest_block_idx()?.map_or(
        SyncRange::new(0, tip_block_index, ByteBuf::from(tip_block_hash), None),
        |block| {
            SyncRange::new(
                // If storage is up to date then the start index is the same as the tip of the ledger.
                block.index + 1,
                tip_block_index,
                ByteBuf::from(tip_block_hash),
                Some(block.block_hash),
            )
        },
    );

    // Do not make a sync call if the storage is up to date with the replica's ledger.
    if !sync_range.index_range.is_empty() {
        sync_blocks_interval(
            agent.clone(),
            storage_client.clone(),
            maximum_blocks_per_request,
            sync_range,
        )
        .await?;
    }
    Ok(())
}

/// Syncs a specific blocks interval, validates it and stores it in storage.
/// Expects the blocks interval to exist on the ledger.
async fn sync_blocks_interval(
    agent: Arc<Icrc1Agent>,
    storage_client: Arc<StorageClient>,
    maximum_blocks_per_request: u64,
    sync_range: SyncRange,
) -> anyhow::Result<()> {
    // Create a progress bar for visualization.
    let pb = ProgressBar::new(*sync_range.index_range.end() - *sync_range.index_range.start() + 1);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] ({eta}) {msg}",
        )
        .unwrap()
        .with_key("eta", |state: &ProgressState, w: &mut dyn Write| {
            write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap()
        })
        .progress_chars("#>-"),
    );

    // The leading index/hash is the highest block index/hash that is requested by the icrc ledger.
    let mut next_index_interval = RangeInclusive::new(
        cmp::max(
            sync_range
                .index_range
                .end()
                .saturating_sub(maximum_blocks_per_request),
            *sync_range.index_range.start(),
        ),
        *sync_range.index_range.end(),
    );
    let mut leading_block_hash = Some(sync_range.leading_block_hash);

    // Start fetching blocks starting from the tip of the blockchain and store them in the
    // database.
    loop {
        // The fetch_blocks_interval function guarantees that all blocks that were asked for are fetched if they exist on the ledger.
        let fetched_blocks =
            fetch_blocks_interval(agent.clone(), next_index_interval.clone()).await?;

        // Verify that the fetched blocks are valid.
        // Leading block hash of a non empty fetched blocks can never be `None` -> Unwrap is safe.
        if !blocks_verifier::is_valid_blockchain(
            &fetched_blocks,
            &leading_block_hash.clone().unwrap(),
        ) {
            // Abort synchronization if blockchain is not valid.
            return Err(anyhow::Error::msg(format!(
                "The fetched blockchain contains invalid blocks in index range {} to {}",
                next_index_interval.start(),
                next_index_interval.end()
            )));
        }

        leading_block_hash = fetched_blocks[0].parent_hash.clone();
        let number_of_blocks_fetched = fetched_blocks.len();
        pb.inc(number_of_blocks_fetched as u64);

        // Store the fetched blocks in the database.
        storage_client.store_blocks(fetched_blocks.clone())?;

        // If the interval of the last iteration started at the target height, then all blocks above and including the target height have been synched.
        if *next_index_interval.start() == *sync_range.index_range.start() {
            // All blocks were fetched, now the parent hash of the lowest block fetched has to match the hash of the highest block in the database or `None` (If database was empty).
            if leading_block_hash == sync_range.trailing_parent_hash {
                break;
            } else {
                return Err(anyhow::Error::msg(format!(
                    "Hash of block {} in database does not match parent hash of fetched block {}",
                    next_index_interval.start().saturating_sub(1),
                    next_index_interval.start()
                )));
            }
        }

        // Set variables for next loop iteration.
        let interval_start = cmp::max(
            next_index_interval
                .start()
                .saturating_sub(number_of_blocks_fetched as u64),
            *sync_range.index_range.start(),
        );
        let interval_end = cmp::max(
            next_index_interval
                .end()
                .saturating_sub(number_of_blocks_fetched as u64),
            *sync_range.index_range.start(),
        );
        next_index_interval = RangeInclusive::new(interval_start, interval_end);
    }
    pb.finish_with_message(format!(
        "Synced Up to block height: {}",
        *sync_range.index_range.end()
    ));
    Ok(())
}

/// Fetches all blocks given a certain interval. The interval is expected to be smaller or equal to the maximum number of blocks than can be requested.
/// Guarantees to return only if all blocks in the given interval were fetched.
async fn fetch_blocks_interval(
    agent: Arc<Icrc1Agent>,
    index_range: RangeInclusive<u64>,
) -> anyhow::Result<Vec<RosettaBlock>> {
    // Construct a hashmap which maps block indices to blocks. Blocks that have not been fetched are `None`.
    let mut fetched_blocks_result: HashMap<u64, Option<RosettaBlock>> = HashMap::new();

    // Initialize fetched blocks map with `None` as no blocks have been fetched yet.
    index_range.for_each(|index| {
        fetched_blocks_result.insert(index, None);
    });

    // Missing blocks are those block indices where the value in the hashmap is missing.
    let missing_blocks = |blocks: &HashMap<u64, Option<RosettaBlock>>| {
        blocks
            .iter()
            .filter_map(
                |(key, value)| {
                    if value.is_none() {
                        Some(*key)
                    } else {
                        None
                    }
                },
            )
            .collect::<Vec<u64>>()
    };

    // Extract all block index intervals that can be fetch.
    let fetchable_intervals = |blocks: &HashMap<u64, Option<RosettaBlock>>| {
        // Get all the missing block indices and sort them.
        let mut missing = missing_blocks(blocks);
        missing.sort();

        // If all blocks have been fetched return an empty vector.
        if missing.is_empty() {
            return vec![];
        }

        let mut block_ranges = vec![];
        let mut start = missing[0];

        // It is possible that the replica returns block intervals that contain patches --> Find all missing indices and aggregate them in the longest consecutive intervals.
        for i in 1..missing.len() {
            if missing[i] != missing[i - 1] + 1 {
                block_ranges.push(RangeInclusive::new(start, missing[i - 1]));
                start = missing[i];
            }
        }
        block_ranges.push(RangeInclusive::new(start, missing[missing.len() - 1]));
        block_ranges
    };

    // Ensure that this function only returns once all blocks have been collected.
    while !missing_blocks(&fetched_blocks_result).is_empty() {
        // Calculate all longest consecutive block index intervals.
        for interval in fetchable_intervals(&fetched_blocks_result) {
            let get_blocks_request = GetBlocksRequest {
                start: Nat::from(*interval.start()),
                // To include the block at end_index we have to add one, since the index starts at 0.
                length: Nat::from(*interval.end() - *interval.start() + 1),
            };

            // Fetch blocks with a given request from the Icrc1Agent
            let blocks_response: GetBlocksResponse =
                agent.get_blocks(get_blocks_request).await.map_err(|_| {
                    let error_msg = format!(
                        "Icrc1Agent could not fetch blocks in interval {} to {}",
                        interval.start().clone(),
                        interval.end().clone()
                    );
                    anyhow::Error::msg(error_msg)
                })?;

            // Convert all Generic Blocks into RosettaBlocks.
            for (index, block) in blocks_response.blocks.into_iter().enumerate() {
                // The index of the RosettaBlock is the starting index of the request plus the position of current block in the response object.
                let block_index = blocks_response
                    .first_index
                    .0
                    .to_u64()
                    .ok_or_else(|| anyhow::Error::msg("Could not convert Nat to u64"))?
                    + index as u64;
                fetched_blocks_result.insert(
                    block_index,
                    Some(RosettaBlock::from_generic_block(block, block_index)?),
                );
            }

            // Fetch all blocks that could not be returned by the ledger directly, from the
            // archive.
            for archive_query in blocks_response.archived_blocks {
                let arg = Encode!(&GetBlocksRequest {
                    start: archive_query.start.clone(),
                    length: archive_query.length,
                })?;
                let archive_response = agent
                    .agent
                    .query(
                        &archive_query.callback.canister_id,
                        &archive_query.callback.method,
                    )
                    .with_arg(arg)
                    .call()
                    .await?;

                let arch_blocks_result = Decode!(&archive_response, BlockRange)?;

                // The archive guarantees that the first index of the blocks it returns is the same as requested.
                let first_index = archive_query
                    .start
                    .0
                    .to_u64()
                    .ok_or_else(|| anyhow::Error::msg("Nat could not be converted to u64"))?;

                // Iterate over the blocks returned from the archive and add them to the hashmap.
                for (index, block) in arch_blocks_result.blocks.into_iter().enumerate() {
                    let block_index = first_index + index as u64;
                    // The index of the RosettaBlock is the starting index of the request plus the position of the current block in the response object.
                    fetched_blocks_result.insert(
                        block_index,
                        Some(RosettaBlock::from_generic_block(block, block_index)?),
                    );
                }
            }
        }
    }

    // Get all the blocks from the hashmap.
    let mut result = fetched_blocks_result
        .into_values()
        .map(|block| {
            block.ok_or_else(|| anyhow::Error::msg("Could not fetch all requested blocks"))
        })
        .collect::<Result<Vec<RosettaBlock>, anyhow::Error>>()?;

    // The blocks may not have been fetched in order.
    result.sort_by(|a, b| a.index.partial_cmp(&b.index).unwrap());

    Ok(result)
}

/// Fetches the data certificate from the ledger and validates it.
/// Returns the tip index and hash of the ledger.
async fn fetch_blockchain_tip_data(agent: Arc<Icrc1Agent>) -> anyhow::Result<(u64, Hash)> {
    // Fetch the data certificate from the icrc ledger
    let data_certificate = agent.get_data_certificate().await.map_err(|err| {
        anyhow::Error::msg(format!(
            "Could not fetch data certificate from ledger: {:?}",
            err
        ))
    })?;

    // Extract the hash tree from the data certificate and deserialize it into a Tree object.
    let hash_tree: MixedHashTree = serde_cbor::from_slice(&data_certificate.hash_tree)
        .map_err(|err| anyhow::Error::msg(err.to_string()))?;

    // Extract the last block index from the hash tree.
    let last_block_index = match hash_tree.lookup(&[b"last_block_index"]) {
        Found(x) => match x {
            MixedHashTree::Leaf(l) => {
                let mut bytes: [u8; 8] = [0u8; 8];
                for (i, e) in l.iter().enumerate() {
                    bytes[i] = *e;
                }
                Ok(u64::from_be_bytes(bytes))
            }
            _ => Err(anyhow::Error::msg(
                "Last block index was found, but MixedHashTree is no a Leaf",
            )),
        },
        _ => Err(anyhow::Error::msg(
            "Last block index was not found in hash tree",
        )),
    }?;

    // Extract the last block hash from the hash tree.
    let last_block_hash = match hash_tree.lookup(&[b"tip_hash"]) {
        Found(x) => match x {
            MixedHashTree::Leaf(l) => {
                let mut bytes: Hash = [0u8; 32];
                for (i, e) in l.iter().enumerate() {
                    bytes[i] = *e;
                }
                Ok(bytes)
            }
            _ => Err(anyhow::Error::msg(
                "Last block hash was found, but MixedHashTree is no a Leaf",
            )),
        },
        _ => Err(anyhow::Error::msg(
            "Last block hash was not found in hash tree",
        )),
    }?;
    Ok((last_block_index, last_block_hash))
}

pub mod blocks_verifier {
    use crate::common::storage::types::RosettaBlock;
    use serde_bytes::ByteBuf;

    pub fn is_valid_blockchain(
        blockchain: &Vec<RosettaBlock>,
        leading_block_hash: &ByteBuf,
    ) -> bool {
        if blockchain.is_empty() {
            return true;
        }

        // Check that the leading block has the block hash that is provided.
        // Safe to call unwrap as the blockchain is guaranteed to have at least one element.
        if blockchain.last().unwrap().block_hash.clone() != leading_block_hash {
            return false;
        }

        let mut parent_hash = Some(blockchain[0].block_hash.clone());
        // The blockchain has more than one element so it is save to skip the first one.
        // The first element cannot be verified so we start at element 2.
        for block in blockchain.iter().skip(1) {
            if block.parent_hash != parent_hash {
                return false;
            }
            parent_hash = Some(block.block_hash.clone());
        }

        // No invalid blocks were found return true.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_icrc1_test_utils::valid_blockchain_strategy;
    use proptest::prelude::*;
    use rand::seq::SliceRandom;
    use serde_bytes::ByteBuf;

    proptest! {
            #[test]
        fn test_valid_blockchain(blockchain in valid_blockchain_strategy(1000)){
            let num_blocks = blockchain.len();
            let mut rosetta_blocks = vec![];
            for (index,block) in blockchain.into_iter().enumerate(){
                rosetta_blocks.push(RosettaBlock::from_icrc_ledger_block(block,index as u64).unwrap());
            }
            // Blockchain is valid and should thus pass the verification.
            assert!(blocks_verifier::is_valid_blockchain(&rosetta_blocks,&rosetta_blocks.last().map(|block|block.block_hash.clone()).unwrap_or_else(|| ByteBuf::from(r#"TestBytes"#))));

            // There is no point in shuffling the blockchain if it has length zero.
            if num_blocks > 0 {
                // If shuffled, the blockchain is no longer in order and thus no longer valid.
                rosetta_blocks.shuffle(&mut rand::thread_rng());
                let shuffled_blocks = rosetta_blocks.to_vec();
                assert!(!blocks_verifier::is_valid_blockchain(&shuffled_blocks,&rosetta_blocks.last().unwrap().block_hash.clone())|| num_blocks<=1||rosetta_blocks==shuffled_blocks);
            }

        }
    }
}
