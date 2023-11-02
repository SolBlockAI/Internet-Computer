use std::time::Duration;

use crate::boundary_nodes::{constants::BOUNDARY_NODE_NAME, helpers::BoundaryNodeHttpsConfig};
use crate::driver::farm::HostFeature;
use crate::driver::ic::{AmountOfMemoryKiB, ImageSizeGiB, NrOfVCPUs, VmResources};
use crate::util::block_on;
use crate::{
    canister_api::{CallMode, GenericRequest},
    driver::{
        boundary_node::{BoundaryNode, BoundaryNodeVm},
        ic::{InternetComputer, Subnet},
        prometheus_vm::{HasPrometheus, PrometheusVm},
        test_env::TestEnv,
        test_env_api::{
            retry_async, HasPublicApiUrl, HasTopologySnapshot, IcNodeContainer,
            NnsInstallationBuilder, RetrieveIpv4Addr, SshSession, READY_WAIT_TIMEOUT,
            RETRY_BACKOFF,
        },
    },
    util::spawn_round_robin_workload_engine,
};
use anyhow::Context;
use ic_protobuf::registry::routing_table::v1::RoutingTable as PbRoutingTable;
use ic_registry_keys::make_routing_table_record_key;
use ic_registry_nns_data_provider::registry::RegistryCanister;
use ic_registry_routing_table::RoutingTable;
use ic_registry_subnet_type::SubnetType;
use prost::Message;
use slog::info;

const COUNTER_CANISTER_WAT: &str = "rs/tests/src/counter.wat";
// Size of the payload sent to the counter canister in update("write") call.
const PAYLOAD_SIZE_BYTES: usize = 1024;
// Parameters related to workload creation.
const REQUESTS_DISPATCH_EXTRA_TIMEOUT: Duration = Duration::from_secs(1);

pub fn setup(bn_https_config: BoundaryNodeHttpsConfig, env: TestEnv) {
    let logger = env.logger();
    PrometheusVm::default()
        .with_required_host_features(vec![HostFeature::Performance])
        .start(&env)
        .expect("failed to start prometheus VM");
    InternetComputer::new()
        .with_required_host_features(vec![HostFeature::Performance])
        .add_subnet(Subnet::new(SubnetType::System).add_nodes(1))
        .add_subnet(
            Subnet::new(SubnetType::Application)
                .with_default_vm_resources(VmResources {
                    vcpus: Some(NrOfVCPUs::new(64)),
                    memory_kibibytes: Some(AmountOfMemoryKiB::new(512_142_680)),
                    boot_image_minimal_size_gibibytes: Some(ImageSizeGiB::new(500)),
                })
                .add_nodes(1),
        )
        .setup_and_start(&env)
        .expect("failed to setup IC under test");
    let nns_node = env
        .topology_snapshot()
        .root_subnet()
        .nodes()
        .next()
        .unwrap();
    NnsInstallationBuilder::new()
        .install(&nns_node, &env)
        .expect("Could not install NNS canisters");
    let bn = BoundaryNode::new(String::from(BOUNDARY_NODE_NAME))
        .with_required_host_features(vec![HostFeature::Performance])
        .with_vm_resources(VmResources {
            // We actually use 15 vCPUs in prod, but Farm complains about CPU topology when this number is not 2^N.
            vcpus: Some(NrOfVCPUs::new(16)),
            memory_kibibytes: Some(AmountOfMemoryKiB::new(16777216)),
            boot_image_minimal_size_gibibytes: None,
        })
        .allocate_vm(&env)
        .unwrap()
        .for_ic(&env, "");
    let bn = match bn_https_config {
        BoundaryNodeHttpsConfig::UseRealCertsAndDns => bn.use_real_certs_and_dns(),
        BoundaryNodeHttpsConfig::AcceptInvalidCertsAndResolveClientSide => bn,
    };
    bn.start(&env).expect("failed to setup BoundaryNode VM");
    // Await Replicas
    info!(&logger, "Checking readiness of all replica nodes...");
    for subnet in env.topology_snapshot().subnets() {
        for node in subnet.nodes() {
            node.await_status_is_healthy()
                .expect("Replica did not come up healthy.");
        }
    }
    info!(&logger, "Polling registry");
    let registry = RegistryCanister::new(bn.nns_node_urls);
    let (latest, routes) = block_on(retry_async(&logger, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
        let (bytes, latest) = registry.get_value(make_routing_table_record_key().into(), None).await
            .context("Failed to `get_value` from registry")?;
        let routes = PbRoutingTable::decode(bytes.as_slice())
            .context("Failed to decode registry routes")?;
        let routes = RoutingTable::try_from(routes)
            .context("Failed to convert registry routes")?;
        Ok((latest, routes))
    }))
    .expect("Failed to poll registry. This is not a Boundary Node error. It is a test environment issue.");
    info!(&logger, "Latest registry {latest}: {routes:?}");
    // Await Boundary Node
    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();
    info!(
        &logger,
        "Boundary node {BOUNDARY_NODE_NAME} has IPv6 {:?}",
        boundary_node.ipv6()
    );
    info!(
        &logger,
        "Boundary node {BOUNDARY_NODE_NAME} has IPv4 {:?}",
        boundary_node.block_on_ipv4().unwrap()
    );
    info!(&logger, "Waiting for routes file");
    let routes_path = "/var/opt/nginx/ic/ic_routes.js";
    let sleep_command = format!("while grep -q '// PLACEHOLDER' {routes_path}; do sleep 5; done");
    let cmd_output = boundary_node.block_on_bash_script(&sleep_command).unwrap();
    info!(
        logger,
        "{BOUNDARY_NODE_NAME} ran `{sleep_command}`: '{}'",
        cmd_output.trim(),
    );
    info!(&logger, "Checking BN health");
    boundary_node
        .await_status_is_healthy()
        .expect("Boundary node did not come up healthy.");
    env.sync_with_prometheus();
}

// Execute update calls (without polling) with an increasing req/s rate, against a counter canister via the boundary node agent.
// At the moment 300 req/s is the maximum defined by the rate limiter in /ic-os/boundary-guestos/rootfs/etc/nginx/conf.d/000-nginx-global.conf

pub fn update_calls_test(env: TestEnv) {
    let rps_min = 50;
    let rps_max = 450;
    let rps_step = 50;
    let workload_per_step_duration = Duration::from_secs(60 * 4);
    let log: slog::Logger = env.logger();
    let subnet_app = env
        .topology_snapshot()
        .subnets()
        .find(|s| s.subnet_type() == SubnetType::Application)
        .unwrap();
    let canister_app = subnet_app
        .nodes()
        .next()
        .unwrap()
        .create_and_install_canister_with_arg(COUNTER_CANISTER_WAT, None);
    let bn_agent = {
        let boundary_node = env
            .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
            .unwrap()
            .get_snapshot()
            .unwrap();
        boundary_node.build_default_agent()
    };
    let payload: Vec<u8> = vec![0; PAYLOAD_SIZE_BYTES];
    for rps in (rps_min..=rps_max).rev().step_by(rps_step) {
        let agent = bn_agent.clone();
        info!(
            log,
            "Starting workload with rps={rps} for {} sec",
            workload_per_step_duration.as_secs()
        );
        let handle_workload = {
            let requests = vec![GenericRequest::new(
                canister_app,
                "write".to_string(),
                payload.clone(),
                CallMode::UpdateNoPolling,
            )];
            spawn_round_robin_workload_engine(
                log.clone(),
                requests,
                vec![agent.clone()],
                rps,
                workload_per_step_duration,
                REQUESTS_DISPATCH_EXTRA_TIMEOUT,
                vec![],
            )
        };
        let metrics = handle_workload.join().expect("Workload execution failed.");
        info!(&log, "Workload metrics for rps={rps}: {metrics}");
        info!(
            log,
            "Failed/successful requests count {}/{}",
            metrics.failure_calls(),
            metrics.success_calls()
        );
        let expected_requests_count = rps * workload_per_step_duration.as_secs() as usize;
        assert_eq!(metrics.success_calls(), expected_requests_count);
        assert_eq!(metrics.failure_calls(), 0);
    }
}

// Execute query calls with an increasing req/s rate, against a counter canister via the boundary node agent.
// In order to observe rates>1 req/s on the replica, caching should be disabled in /ic-os/boundary-guestos/rootfs/etc/nginx/conf.d/002-mainnet-nginx.conf

pub fn query_calls_test(env: TestEnv) {
    let rps_min = 500;
    let rps_max = 6500;
    let rps_step = 500;
    let workload_per_step_duration = Duration::from_secs(60 * 4);
    let log: slog::Logger = env.logger();
    let subnet_app = env
        .topology_snapshot()
        .subnets()
        .find(|s| s.subnet_type() == SubnetType::Application)
        .unwrap();
    let canister_app = subnet_app
        .nodes()
        .next()
        .unwrap()
        .create_and_install_canister_with_arg(COUNTER_CANISTER_WAT, None);
    let bn_agent = {
        let boundary_node = env
            .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
            .unwrap()
            .get_snapshot()
            .unwrap();
        boundary_node.build_default_agent()
    };
    let payload: Vec<u8> = vec![0; PAYLOAD_SIZE_BYTES];
    for rps in (rps_min..=rps_max).rev().step_by(rps_step) {
        let agent = bn_agent.clone();
        info!(
            log,
            "Starting workload with rps={rps} for {} sec",
            workload_per_step_duration.as_secs()
        );
        let handle_workload = {
            let requests = vec![GenericRequest::new(
                canister_app,
                "read".to_string(),
                payload.clone(),
                CallMode::Query,
            )];
            spawn_round_robin_workload_engine(
                log.clone(),
                requests,
                vec![agent.clone()],
                rps,
                workload_per_step_duration,
                REQUESTS_DISPATCH_EXTRA_TIMEOUT,
                vec![],
            )
        };
        let metrics = handle_workload.join().expect("Workload execution failed.");
        info!(&log, "Workload metrics for rps={rps}: {metrics}");
        info!(
            log,
            "Failed/successful requests count {}/{}",
            metrics.failure_calls(),
            metrics.success_calls()
        );
        let expected_requests_count = rps * workload_per_step_duration.as_secs() as usize;
        assert_eq!(metrics.success_calls(), expected_requests_count);
        assert_eq!(metrics.failure_calls(), 0);
    }
}
