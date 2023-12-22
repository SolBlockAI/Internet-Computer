use crate::consensus::U64Artifact;
use async_trait::async_trait;
use axum::http::{Request, Response};
use bytes::Bytes;
use ic_interfaces::p2p::{
    consensus::{PriorityFnAndFilterProducer, ValidatedPoolReader},
    state_sync::StateSyncClient,
};
use ic_quic_transport::{ConnId, SendError, Transport};
use ic_types::{artifact::PriorityFn, chunkable::ArtifactChunk};
use ic_types::{
    artifact::{StateSyncArtifactId, StateSyncMessage},
    chunkable::{ArtifactErrorCode, Chunkable},
    chunkable::{Chunk, ChunkId},
    NodeId,
};
use mockall::mock;

mock! {
    pub StateSync {}

    impl StateSyncClient for StateSync {
        fn available_states(&self) -> Vec<StateSyncArtifactId>;

        fn start_state_sync(
            &self,
            id: &StateSyncArtifactId,
        ) -> Option<Box<dyn Chunkable + Send + Sync>>;

        fn should_cancel(&self, id: &StateSyncArtifactId) -> bool;

        fn chunk(&self, id: &StateSyncArtifactId, chunk_id: ChunkId) -> Option<Chunk>;

        fn deliver_state_sync(&self, msg: StateSyncMessage);
    }
}

mock! {
    pub Transport {}

    #[async_trait]
    impl Transport for Transport{
        async fn rpc(
            &self,
            peer_id: &NodeId,
            request: Request<Bytes>,
        ) -> Result<Response<Bytes>, SendError>;

        async fn push(
            &self,
            peer_id: &NodeId,
            request: Request<Bytes>,
        ) -> Result<(), SendError>;

        fn peers(&self) -> Vec<(NodeId, ConnId)>;
    }
}

mock! {
    pub Chunkable {}

    impl Chunkable for Chunkable{
        fn chunks_to_download(&self) -> Box<dyn Iterator<Item = ChunkId>>;
        fn add_chunk(&mut self, artifact_chunk: ArtifactChunk) -> Result<StateSyncMessage, ArtifactErrorCode>;
    }
}

mock! {
    pub ValidatedPoolReader {}

    impl ValidatedPoolReader<U64Artifact> for ValidatedPoolReader {
        fn contains(&self, id: &u64) -> bool;
        fn get_validated_by_identifier(&self, id: &u64) -> Option<u64>;
        fn get_all_validated_by_filter(
            &self,
            filter: &(),
        ) -> Box<dyn Iterator<Item = u64>>;
    }
}

mock! {
    pub PriorityFnAndFilterProducer {}

    impl PriorityFnAndFilterProducer<U64Artifact, MockValidatedPoolReader > for PriorityFnAndFilterProducer {
        fn get_priority_function(&self, pool: &MockValidatedPoolReader) -> PriorityFn<u64, ()>;
        fn get_filter(&self) -> () {
            ()
        }

    }
}
