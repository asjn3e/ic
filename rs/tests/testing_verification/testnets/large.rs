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

use std::time::Duration;

use anyhow::Result;

use ic_consensus_system_test_utils::rw_message::install_nns_with_customizations_and_check_progress;
use ic_registry_subnet_type::SubnetType;
use ic_system_test_driver::driver::farm::HostFeature;
use ic_system_test_driver::driver::ic::{
    AmountOfMemoryKiB, ImageSizeGiB, InternetComputer, NrOfVCPUs, Subnet, VmResources,
};
use ic_system_test_driver::driver::{
    boundary_node::BoundaryNode,
    group::SystemTestGroup,
    prometheus_vm::{HasPrometheus, PrometheusVm},
    simulate_network::{simulate_network, NetworkSimulation, ProductionSubnetTopology},
    test_env::TestEnv,
    test_env_api::{
        await_boundary_node_healthy, HasTopologySnapshot, IcNodeContainer, NnsCanisterWasmStrategy,
    },
};
use ic_system_test_driver::sns_client::add_all_wasms_to_sns_wasm;
use ic_tests::nns_dapp::{
    install_ii_nns_dapp_and_subnet_rental, install_sns_aggregator, nns_dapp_customizations,
    set_authorized_subnets, set_icp_xdr_exchange_rate, set_sns_subnet,
};

const NUM_NODES_FULL_CONSENSUS_APP_SUBNET: usize = 13;
const DOWNLOAD_PROMETHEUS_WAIT_TIME: Duration = Duration::from_secs(4 * 60 * 60);

const NETWORK_SIMULATION: NetworkSimulation =
    NetworkSimulation::Subnet(ProductionSubnetTopology::IO67);

fn main() -> Result<()> {
    SystemTestGroup::new()
        .with_setup(setup)
        .execute_from_args()?;
    Ok(())
}

pub fn setup(env: TestEnv) {
    // start p8s for metrics and dashboards
    PrometheusVm::default()
        .start(&env)
        .expect("Failed to start prometheus VM");

    // set up IC overriding the default resources to be more powerful
    let vm_resources = VmResources {
        vcpus: Some(NrOfVCPUs::new(48)),
        memory_kibibytes: Some(AmountOfMemoryKiB::new(128 * 1024 * 1024)), // <- 128 GB
        boot_image_minimal_size_gibibytes: Some(ImageSizeGiB::new(2000)),
    };

    InternetComputer::new()
        .with_default_vm_resources(vm_resources)
        .with_required_host_features(vec![HostFeature::Performance])
        .add_subnet(
            Subnet::new(SubnetType::Application).add_nodes(NUM_NODES_FULL_CONSENSUS_APP_SUBNET),
        )
        .setup_and_start(&env)
        .expect("Failed to setup IC under test");

    let app_subnet = env
        .topology_snapshot()
        .subnets()
        .find(|s| s.subnet_type() == SubnetType::Application)
        .unwrap();

    println!("Setting topologies.");
    simulate_network(app_subnet, &NETWORK_SIMULATION);
    println!("Topologies set.");

    std::thread::sleep(DOWNLOAD_PROMETHEUS_WAIT_TIME);
    env.download_prometheus_data_dir_if_exists();
}
