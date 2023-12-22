// Set up a testnet containing:
//   one 4-node System, one 4-node Application, and one 1-node Application subnets, a single boundary node, and a p8s (with grafana) VM.
// All replica nodes use the following resources: 64 vCPUs, 480GiB of RAM, and 2,000 GiB disk.
//
// You can setup this testnet with a lifetime of 180 mins by executing the following commands:
//
//   $ ./gitlab-ci/tools/docker-run
//   $ ict testnet create large --lifetime-mins=180 --output-dir=./large -- --test_tmpdir=./large
//
// The --output-dir=./large will store the debug output of the test driver in the specified directory.
// The --test_tmpdir=./large will store the remaining test output in the specified directory.
// This is useful to have access to in case you need to SSH into an IC node for example like:
//
//   $ ssh -i large/_tmp/*/setup/ssh/authorized_priv_keys/admin admin@$ipv6
//
// Note that you can get the $ipv6 address of the IC node from the ict console output:
//
//   {
//     "nodes": [
//       {
//         "id": "y4g5e-dpl4n-swwhv-la7ec-32ngk-w7f3f-pr5bt-kqw67-2lmfy-agipc-zae",
//         "ipv6": "2a0b:21c0:4003:2:5034:46ff:fe3c:e76f"
//       },
//       {
//         "id": "df2nt-xpdbh-kekha-igdy2-t2amw-ui36p-dqrte-ojole-syd4u-sfhqz-3ae",
//         "ipv6": "2a0b:21c0:4003:2:50d2:3ff:fe24:32fe"
//       }
//     ],
//     "subnet_id": "5hv4k-srndq-xgw53-r6ldt-wtv4x-6xvbj-6lvpf-sbu5n-sqied-63bgv-eqe",
//     "subnet_type": "application"
//   },
//
// To get access to P8s and Grafana look for the following lines in the ict console output:
//
//     "prometheus": "Prometheus Web UI at http://prometheus.large--1692597750709.testnet.farm.dfinity.systems",
//     "grafana": "Grafana at http://grafana.large--1692597750709.testnet.farm.dfinity.systems",
//     "progress_clock": "IC Progress Clock at http://grafana.large--1692597750709.testnet.farm.dfinity.systems/d/ic-progress-clock/ic-progress-clock?refresh=10s\u0026from=now-5m\u0026to=now",
//
// Happy testing!

use anyhow::Result;

use ic_registry_subnet_type::SubnetType;
use ic_tests::driver::ic::{
    AmountOfMemoryKiB, ImageSizeGiB, InternetComputer, NrOfVCPUs, Subnet, VmResources,
};
use ic_tests::driver::{
    boundary_node::BoundaryNode,
    group::SystemTestGroup,
    prometheus_vm::{HasPrometheus, PrometheusVm},
    test_env::TestEnv,
    test_env_api::{await_boundary_node_healthy, HasTopologySnapshot, NnsCanisterWasmStrategy},
};
use ic_tests::nns_dapp::{
    install_ii_and_nns_dapp, nns_dapp_customizations, set_authorized_subnets,
};
use ic_tests::orchestrator::utils::rw_message::install_nns_with_customizations_and_check_progress;

const NUM_FULL_CONSENSUS_APP_SUBNETS: u64 = 1;
const NUM_SINGLE_NODE_APP_SUBNETS: u64 = 1;
const NUM_BN: u64 = 1;

fn main() -> Result<()> {
    SystemTestGroup::new()
        .with_setup(setup)
        .execute_from_args()?;
    Ok(())
}

pub fn setup(env: TestEnv) {
    PrometheusVm::default()
        .start(&env)
        .expect("Failed to start prometheus VM");
    let vm_resources = VmResources {
        vcpus: Some(NrOfVCPUs::new(64)),
        memory_kibibytes: Some(AmountOfMemoryKiB::new(480 << 20)),
        boot_image_minimal_size_gibibytes: Some(ImageSizeGiB::new(2000)),
    };
    let mut ic = InternetComputer::new().with_default_vm_resources(vm_resources);
    ic = ic.add_subnet(Subnet::new(SubnetType::System).add_nodes(4));
    for _ in 0..NUM_FULL_CONSENSUS_APP_SUBNETS {
        ic = ic.add_subnet(Subnet::new(SubnetType::Application).add_nodes(4));
    }
    for _ in 0..NUM_SINGLE_NODE_APP_SUBNETS {
        ic = ic.add_subnet(Subnet::new(SubnetType::Application).add_nodes(1));
    }
    ic.setup_and_start(&env)
        .expect("Failed to setup IC under test");
    install_nns_with_customizations_and_check_progress(
        env.topology_snapshot(),
        NnsCanisterWasmStrategy::TakeBuiltFromSources,
        nns_dapp_customizations(),
    );
    set_authorized_subnets(&env);
    for i in 0..NUM_BN {
        let bn_name = format!("boundary-node-{}", i);
        BoundaryNode::new(bn_name)
            .allocate_vm(&env)
            .expect("Allocation of BoundaryNode failed.")
            .for_ic(&env, "")
            .use_real_certs_and_dns()
            .start(&env)
            .expect("failed to setup BoundaryNode VM");
    }
    env.sync_with_prometheus();
    for i in 0..NUM_BN {
        let bn_name = format!("boundary-node-{}", i);
        await_boundary_node_healthy(&env, &bn_name);
        if i == 0 {
            install_ii_and_nns_dapp(&env, &bn_name, None);
        }
    }
}
