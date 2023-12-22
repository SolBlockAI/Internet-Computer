use ic_base_types::{NodeId, RegistryVersion};
use ic_crypto_tls_interfaces::{
    SomeOrAllNodes, TlsClientHandshakeError, TlsHandshake, TlsServerHandshakeError,
};
use ic_crypto_tls_interfaces_mocks::MockTlsHandshake;
use ic_interfaces_transport::TransportEvent;
use ic_test_utilities_logger::with_test_replica_logger;
use ic_transport_test_utils::{
    create_mock_event_handler, get_free_localhost_port, setup_test_peer,
    temp_crypto_component_with_tls_keys_in_registry, RegistryAndDataProvider, NODE_ID_1, NODE_ID_2,
    REG_V1,
};
use std::sync::Arc;
use tokio::net::TcpStream;

#[test]
fn test_single_transient_failure_of_tls_client_handshake_legacy() {
    test_single_transient_failure_of_tls_client_handshake_impl(false);
}

#[test]
fn test_single_transient_failure_of_tls_client_handshake_h2() {
    test_single_transient_failure_of_tls_client_handshake_impl(true);
}

fn test_single_transient_failure_of_tls_client_handshake_impl(use_h2: bool) {
    with_test_replica_logger(|log| {
        let mut registry_and_data = RegistryAndDataProvider::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rt_handle = rt.handle().clone();

        let crypto_factory_with_single_tls_handshake_client_failures =
            |registry_and_data: &mut RegistryAndDataProvider, node_id: NodeId| {
                let mut mock_client_tls_handshake = MockTlsHandshake::new();
                let rt_handle = rt_handle.clone();

                let crypto = Arc::new(temp_crypto_component_with_tls_keys_in_registry(
                    registry_and_data,
                    node_id,
                ));

                mock_client_tls_handshake
                    .expect_perform_tls_client_handshake()
                    .times(1)
                    .returning({
                        move |_tcp_stream: TcpStream,
                              _server: NodeId,
                              _registry_version: RegistryVersion| {
                            Err(TlsClientHandshakeError::HandshakeError {
                                internal_error: "transient".to_string(),
                            })
                        }
                    });

                mock_client_tls_handshake
                    .expect_perform_tls_client_handshake()
                    .times(1)
                    .returning(
                        move |tcp_stream: TcpStream,
                              server: NodeId,
                              registry_version: RegistryVersion| {
                            let rt_handle = rt_handle.clone();
                            let crypto = crypto.clone();
                            #[allow(clippy::disallowed_methods)]
                            tokio::task::block_in_place(move || {
                                let rt_handle = rt_handle.clone();

                                rt_handle.block_on(async move {
                                    crypto
                                        .perform_tls_client_handshake(
                                            tcp_stream,
                                            server,
                                            registry_version,
                                        )
                                        .await
                                })
                            })
                        },
                    );

                Arc::new(mock_client_tls_handshake) as Arc<dyn TlsHandshake + Send + Sync>
            };

        let crypto_factory = |registry_and_data: &mut RegistryAndDataProvider, node_id: NodeId| {
            Arc::new(temp_crypto_component_with_tls_keys_in_registry(
                registry_and_data,
                node_id,
            )) as Arc<dyn TlsHandshake + Send + Sync>
        };

        let peer1_port = get_free_localhost_port().expect("Failed to get free localhost port");
        let (event_handler_1, mut handle_1) = create_mock_event_handler();

        let (peer_1, peer_1_addr) = setup_test_peer(
            log.clone(),
            rt.handle().clone(),
            NODE_ID_1,
            peer1_port,
            REG_V1,
            &mut registry_and_data,
            crypto_factory_with_single_tls_handshake_client_failures,
            event_handler_1,
            use_h2,
        );
        let peer2_port = get_free_localhost_port().expect("Failed to get free localhost port");
        let (event_handler_2, mut handle_2) = create_mock_event_handler();

        let (peer_2, peer_2_addr) = setup_test_peer(
            log,
            rt.handle().clone(),
            NODE_ID_2,
            peer2_port,
            REG_V1,
            &mut registry_and_data,
            crypto_factory,
            event_handler_2,
            use_h2,
        );
        registry_and_data.registry.update_to_latest_version();

        peer_1.start_connection(&NODE_ID_2, peer_2_addr, REG_V1, REG_V1);
        peer_2.start_connection(&NODE_ID_1, peer_1_addr, REG_V1, REG_V1);

        rt.block_on(async {
            match handle_1.next_request().await {
                Some((TransportEvent::PeerUp(_), resp)) => {
                    resp.send_response(());
                }
                _ => panic!("Unexpected event"),
            }
            match handle_2.next_request().await {
                Some((TransportEvent::PeerUp(_), resp)) => {
                    resp.send_response(());
                }
                _ => panic!("Unexpected event"),
            }
        });
        // We stop the connection _from_ the peer(client) with the mocked TLS handshake
        // object in order not to track now many times particular methods are called on
        // reconnects.
        peer_1.stop_connection(&NODE_ID_2);

        rt.block_on(async {
            match handle_2.next_request().await {
                Some((TransportEvent::PeerDown(_), resp)) => {
                    resp.send_response(());
                }
                _ => panic!("Unexpected event"),
            }
        });
    });
}

#[test]
fn test_single_transient_failure_of_tls_server_handshake_legacy() {
    test_single_transient_failure_of_tls_server_handshake_impl(false);
}
#[test]
fn test_single_transient_failure_of_tls_server_handshake_h2() {
    test_single_transient_failure_of_tls_server_handshake_impl(true);
}

fn test_single_transient_failure_of_tls_server_handshake_impl(use_h2: bool) {
    with_test_replica_logger(|log| {
        let mut registry_and_data = RegistryAndDataProvider::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rt_handle = rt.handle().clone();

        let crypto_factory_with_single_tls_handshake_server_failures =
            |registry_and_data: &mut RegistryAndDataProvider, node_id: NodeId| {
                let mut mock_server_tls_handshake = MockTlsHandshake::new();
                let rt_handle = rt_handle.clone();

                let crypto = Arc::new(temp_crypto_component_with_tls_keys_in_registry(
                    registry_and_data,
                    node_id,
                ));

                mock_server_tls_handshake
                    .expect_perform_tls_server_handshake()
                    .times(1)
                    .returning({
                        move |_tcp_stream: TcpStream,
                              _allowed_clients: SomeOrAllNodes,
                              _registry_version: RegistryVersion| {
                            Err(TlsServerHandshakeError::HandshakeError {
                                internal_error: "transient".to_string(),
                            })
                        }
                    });

                mock_server_tls_handshake
                    .expect_perform_tls_server_handshake()
                    .times(1)
                    .returning(
                        move |tcp_stream: TcpStream,
                              allowed_clients: SomeOrAllNodes,
                              registry_version: RegistryVersion| {
                            let rt_handle = rt_handle.clone();
                            let crypto = crypto.clone();
                            #[allow(clippy::disallowed_methods)]
                            tokio::task::block_in_place(move || {
                                let rt_handle = rt_handle.clone();

                                rt_handle.block_on(async move {
                                    crypto
                                        .perform_tls_server_handshake(
                                            tcp_stream,
                                            allowed_clients,
                                            registry_version,
                                        )
                                        .await
                                })
                            })
                        },
                    );

                Arc::new(mock_server_tls_handshake) as Arc<dyn TlsHandshake + Send + Sync>
            };

        let crypto_factory = |registry_and_data: &mut RegistryAndDataProvider, node_id: NodeId| {
            Arc::new(temp_crypto_component_with_tls_keys_in_registry(
                registry_and_data,
                node_id,
            )) as Arc<dyn TlsHandshake + Send + Sync>
        };

        let peer1_port = get_free_localhost_port().expect("Failed to get free localhost port");
        let (event_handler_1, mut handle_1) = create_mock_event_handler();
        let (peer_1, peer_1_addr) = setup_test_peer(
            log.clone(),
            rt.handle().clone(),
            NODE_ID_1,
            peer1_port,
            REG_V1,
            &mut registry_and_data,
            crypto_factory,
            event_handler_1,
            use_h2,
        );
        let peer2_port = get_free_localhost_port().expect("Failed to get free localhost port");
        let (event_handler_2, mut handle_2) = create_mock_event_handler();
        let (peer_2, peer_2_addr) = setup_test_peer(
            log,
            rt.handle().clone(),
            NODE_ID_2,
            peer2_port,
            REG_V1,
            &mut registry_and_data,
            crypto_factory_with_single_tls_handshake_server_failures,
            event_handler_2,
            use_h2,
        );
        registry_and_data.registry.update_to_latest_version();

        peer_1.start_connection(&NODE_ID_2, peer_2_addr, REG_V1, REG_V1);
        peer_2.start_connection(&NODE_ID_1, peer_1_addr, REG_V1, REG_V1);
        rt.block_on(async {
            match handle_1.next_request().await {
                Some((TransportEvent::PeerUp(_), resp)) => {
                    resp.send_response(());
                }
                _ => panic!("Unexpected event"),
            }
            match handle_2.next_request().await {
                Some((TransportEvent::PeerUp(_), resp)) => {
                    resp.send_response(());
                }
                _ => panic!("Unexpected event"),
            }
        });
        // We stop the connection _to_ the peer(server) the the mocked TLS handshake
        // object in order not to track now many times particular methods are called on
        // reconnects.
        peer_1.stop_connection(&NODE_ID_2);

        rt.block_on(async {
            match handle_2.next_request().await {
                Some((TransportEvent::PeerDown(_), resp)) => {
                    resp.send_response(());
                }
                _ => panic!("Unexpected event"),
            }
        });
    });
}
