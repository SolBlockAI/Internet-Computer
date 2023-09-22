use ic_interfaces_transport::{
    Transport, TransportChannelId, TransportError, TransportEvent, TransportEventHandler,
    TransportMessage, TransportPayload,
};
use ic_logger::{info, ReplicaLogger};
use ic_types::{NodeId, RegistryVersion};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock, Weak};
use tower::Service;

#[derive(Default)]
struct Deferred {
    // messages that cannot be delivered until client is registered
    // and gossip calls start nodes.
    stash: Vec<TransportPayload>,
    started: bool,
}

pub struct ThreadPort {
    id: NodeId,
    // Access to full hub to route messages across threads
    hub_access: HubAccess,
    event_handler: Mutex<Option<TransportEventHandler>>,
    log: ReplicaLogger,
    deferred: Mutex<HashMap<NodeId, Deferred>>,
    weak_self: RwLock<Weak<ThreadPort>>,
    rt_handle: tokio::runtime::Handle,
}

impl Debug for ThreadPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({:?}, {:?})", self.id, self.hub_access)
    }
}

#[allow(dead_code)]
impl ThreadPort {
    pub fn new(
        id: NodeId,
        hub_access: HubAccess,
        log: ReplicaLogger,
        rt_handle: tokio::runtime::Handle,
    ) -> Arc<Self> {
        let thread_port = Arc::new(Self {
            id,
            hub_access,
            event_handler: Mutex::new(None),
            log,
            deferred: Default::default(),
            weak_self: RwLock::new(Weak::new()),
            rt_handle,
        });
        *thread_port.weak_self.write().unwrap() = Arc::downgrade(&thread_port);
        thread_port
    }

    fn replay_deferred(&self, node_id: NodeId) {
        let replay = {
            let mut deferred_guard = self.deferred.lock();
            let deferred_map = &mut *deferred_guard;
            let mut replay = Vec::new();
            let i = 0;
            let deferred = deferred_map.entry(node_id).or_default();
            while i != deferred.stash.len() {
                replay.push(deferred.stash.swap_remove(i));
            }
            deferred.started = true;
            replay
        };

        let weak_self = self.weak_self.read().unwrap().clone();
        let arc_self = weak_self.upgrade().unwrap();
        for elt in replay.into_iter() {
            info!(
                self.log,
                "Replaying deferred message {:?}: From node {:?} to node {:?}",
                elt.clone(),
                node_id,
                self.id
            );

            let arc_self = arc_self.clone();
            let id = self.id;
            self.rt_handle.spawn(async move {
                if arc_self
                    .send_helper(node_id, id, elt.clone())
                    .await
                    .is_err()
                {
                    println!("!!! Send failed !!!");
                }
            });
        }
    }

    // More expressive send helper,  Allows to explicitly specify send and receive
    // node ids
    async fn send_helper(
        &self,
        src_node_id: NodeId,
        dest_node_id: NodeId,
        message: TransportPayload,
    ) -> Result<(), TransportError> {
        // Dispatch  or defer send a message to a node.
        // Dispatch happens only if all 3 conditions are met
        //.
        // 1. Destination  node has installed a port. i.e. destination node has created
        // Thread port.
        //
        // 2. Destination port has registered the client event handler.
        //
        // 3. Destination node has called start connections.
        //
        // For any other case the message processing deferred until
        // the conditions are met.

        // 1.
        let destination_node = {
            let hub_access = self.hub_access.lock();
            // All node ports must be connected  to hub before test start.
            hub_access.ports[&dest_node_id].clone()
        };

        // 2.
        let event_handler = destination_node.event_handler.lock().clone();

        // 3.
        let mut event_handler = {
            let mut deferred = destination_node.deferred.lock();
            let deferred = deferred.entry(src_node_id).or_default();

            // Stash
            if !deferred.started || event_handler.is_none() {
                println!("Node {} is not registered", destination_node.id,);
                deferred.stash.push(message);
                return Ok(());
            } else {
                event_handler.unwrap()
            }
        };

        event_handler
            .call(TransportEvent::Message(TransportMessage {
                peer_id: src_node_id,
                payload: message,
            }))
            .await
            .expect("send message failed");
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct Hub {
    ports: BTreeMap<NodeId, Arc<ThreadPort>>,
}
pub type HubAccess = Arc<Mutex<Hub>>;

impl Hub {
    pub fn insert(&mut self, node: NodeId, port: Arc<ThreadPort>) -> Option<Arc<ThreadPort>> {
        self.ports.insert(node, port)
    }
    pub fn get(&self, node: &NodeId) -> Arc<ThreadPort> {
        self.ports[node].clone()
    }
}

impl Transport for ThreadPort {
    fn set_event_handler(&self, event_handler: TransportEventHandler) {
        info!(self.log, "Node{} -> Client Registered", self.id);
        *self.event_handler.lock() = Some(event_handler);
    }

    fn start_connection(
        &self,
        node_id: &NodeId,
        _peer_addr: SocketAddr,
        _latest_registry_version: RegistryVersion,
        _earliest_registry_version: RegistryVersion,
    ) {
        info!(
            self.log,
            "Node{} -> Connections to peer {} started", self.id, node_id
        );
        self.replay_deferred(*node_id);
    }

    /// Remove the peer from the set of valid neighbors, and tear down the
    /// queues and connections for the peer. Any messages in the Tx and Rx
    /// queues for the peer will be discarded.
    fn stop_connection(&self, peer_id: &NodeId) {
        info!(
            self.log,
            "Node{} -> Connections to peer {} stopped", self.id, *peer_id
        );
    }

    /// Send the message to the specified peer. The message will be en-queued
    /// into the appropriate TxQ based on the TransportQueueConfig.
    fn send(
        &self,
        peer_id: &NodeId,
        _channel_id: TransportChannelId,
        message: TransportPayload,
    ) -> Result<(), TransportError> {
        let peer_id = *peer_id;
        let id = self.id;
        let weak_self = self.weak_self.read().unwrap().clone();
        let arc_self = weak_self.upgrade().unwrap();
        self.rt_handle
            .spawn(async move { arc_self.send_helper(id, peer_id, message).await });
        Ok(())
    }

    fn clear_send_queues(&self, _peer_id: &NodeId) {}
}
