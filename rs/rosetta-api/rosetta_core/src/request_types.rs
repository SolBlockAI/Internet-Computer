use crate::identifiers::*;
use crate::objects::*;
use serde::{Deserialize, Serialize};

/// A MetadataRequest is utilized in any request where the only argument is
/// optional metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "conversion", derive(LabelledGeneric))]
pub struct MetadataRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Object>,
}

impl MetadataRequest {
    pub fn new() -> MetadataRequest {
        MetadataRequest { metadata: None }
    }
}

/// A NetworkRequest is utilized to retrieve some data specific exclusively to a NetworkIdentifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkRequest {
    /// The network_identifier specifies which network a particular object is associated with.
    pub network_identifier: NetworkIdentifier,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Object>,
}

impl NetworkRequest {
    pub fn new(network_identifier: NetworkIdentifier) -> Self {
        Self {
            network_identifier,
            metadata: None,
        }
    }
}

/// A BlockRequest is utilized to make a block request on the /block endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "conversion", derive(LabelledGeneric))]
pub struct BlockRequest {
    /// The network_identifier specifies which network a particular object is associated with.
    pub network_identifier: NetworkIdentifier,

    /// When fetching data by BlockIdentifier, it may be possible to only specify the index or hash. If neither property is specified, it is assumed that the client is making a request at the current block.
    pub block_identifier: PartialBlockIdentifier,
}

impl BlockRequest {
    pub fn new(
        network_identifier: NetworkIdentifier,
        block_identifier: PartialBlockIdentifier,
    ) -> BlockRequest {
        BlockRequest {
            network_identifier,
            block_identifier,
        }
    }
}

/// A BlockTransactionRequest is used to fetch a Transaction included in a block
/// that is not returned in a BlockResponse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "conversion", derive(LabelledGeneric))]
pub struct BlockTransactionRequest {
    /// The network_identifier specifies which network a particular object is associated with.
    pub network_identifier: NetworkIdentifier,

    /// The block_identifier uniquely identifies a block in a particular network.
    pub block_identifier: BlockIdentifier,

    /// The transaction_identifier uniquely identifies a transaction in a particular network and block or in the mempool.
    pub transaction_identifier: TransactionIdentifier,
}

impl BlockTransactionRequest {
    pub fn new(
        network_identifier: NetworkIdentifier,
        block_identifier: BlockIdentifier,
        transaction_identifier: TransactionIdentifier,
    ) -> BlockTransactionRequest {
        BlockTransactionRequest {
            network_identifier,
            block_identifier,
            transaction_identifier,
        }
    }
}

/// A MempoolTransactionRequest is utilized to retrieve a transaction from the
/// mempool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "conversion", derive(LabelledGeneric))]
pub struct MempoolTransactionRequest {
    // The network_identifier specifies which network a particular object is associated with.
    #[serde(rename = "network_identifier")]
    pub network_identifier: NetworkIdentifier,

    // The transaction_identifier uniquely identifies a transaction in a particular network and block or in the mempool.
    #[serde(rename = "transaction_identifier")]
    pub transaction_identifier: TransactionIdentifier,
}

impl MempoolTransactionRequest {
    pub fn new(
        network_identifier: NetworkIdentifier,
        transaction_identifier: TransactionIdentifier,
    ) -> MempoolTransactionRequest {
        MempoolTransactionRequest {
            network_identifier,
            transaction_identifier,
        }
    }
}

/// ConstructionDeriveRequest is passed to the `/construction/derive` endpoint.
/// Network is provided in the request because some blockchains have different
/// address formats for different networks. Metadata is provided in the request
/// because some blockchains allow for multiple address types (i.e. different
/// address for validators vs normal accounts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "conversion", derive(LabelledGeneric))]
pub struct ConstructionDeriveRequest {
    /// The network_identifier specifies which network a particular object is associated with.
    pub network_identifier: NetworkIdentifier,

    /// PublicKey contains a public key byte array for a particular CurveType encoded in hex. Note that there is no PrivateKey struct as this is NEVER the concern of an implementation.
    pub public_key: PublicKey,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ObjectMap>,
}

impl ConstructionDeriveRequest {
    pub fn new(
        network_identifier: NetworkIdentifier,
        public_key: PublicKey,
    ) -> ConstructionDeriveRequest {
        ConstructionDeriveRequest {
            network_identifier,
            public_key,
            metadata: None,
        }
    }
}
