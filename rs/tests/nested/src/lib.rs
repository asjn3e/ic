use std::str::FromStr;
use std::time::Duration;

use canister_test::PrincipalId;
use ic_consensus_system_test_utils::rw_message::install_nns_and_check_progress;
use ic_registry_subnet_type::SubnetType;
use ic_system_test_driver::{
    driver::{
        bootstrap::NestedVersionTarget, ic::InternetComputer, nested::NestedVms, test_env::TestEnv,
        test_env_api::*,
    },
    nns::add_nodes_to_subnet,
    util::block_on,
};
use ic_types::hostos_version::HostosVersion;

use slog::{info, warn};

mod util;
use util::{
    check_hostos_version, elect_hostos_version, setup_nested_vm_for_test, start_nested_vm,
    update_nodes_hostos_version,
};

const HOST_VM_NAME: &str = "host-1";

const NODE_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const NODE_REGISTRATION_BACKOFF: Duration = Duration::from_secs(5);

/// Prepare the environment for nested tests.
/// SetupOS -> HostOS -> GuestOS
pub fn config(env: TestEnv, from: &NestedVersionTarget) {
    let logger = env.logger();

    let principal =
        PrincipalId::from_str("7532g-cd7sa-3eaay-weltl-purxe-qliyt-hfuto-364ru-b3dsz-kw5uz-kqe")
            .unwrap();

    // Setup "testnet"
    let mut ic = InternetComputer::new()
        .add_fast_single_node_subnet(SubnetType::System)
        .with_node_provider(principal)
        .with_node_operator(principal);

    // Handle the initial version for GuestOS. Currently only supports starting from mainnet, malicious, or branch.
    match from {
        NestedVersionTarget::Mainnet => ic = ic.with_mainnet_config(),
        NestedVersionTarget::Branch(false) => (),
        NestedVersionTarget::Branch(true) => {
            warn!(
                logger,
                "Starting GuestOS VMs from '-test' versions is unsupported. Ignoring."
            );
        }
        NestedVersionTarget::Published { .. } => {
            warn!(
                logger,
                "Starting GuestOS VMs from published versions is unsupported. Ignoring."
            );
        }
    }

    ic.setup_and_start(&env)
        .expect("failed to setup IC under test");

    install_nns_and_check_progress(env.topology_snapshot());

    setup_nested_vm_for_test(env, from, HOST_VM_NAME);
}

/// Allow the nested GuestOS to install and launch, and check that it can
/// successfully join the testnet.
pub fn registration(env: TestEnv) {
    let logger = env.logger();

    let initial_topology = block_on(
        env.topology_snapshot()
            .block_for_min_registry_version(ic_types::RegistryVersion::from(1)),
    )
    .unwrap();

    // Check that there are initially no unassigned nodes.
    let num_unassigned_nodes = initial_topology.unassigned_nodes().count();
    assert_eq!(num_unassigned_nodes, 0);

    start_nested_vm(env);

    // If the node is able to join successfully, the registry will be updated,
    // and the new node ID will enter the unassigned pool.
    info!(logger, "Waiting for node to join ...");
    let new_topology = block_on(
        initial_topology.block_for_newer_registry_version_within_duration(
            NODE_REGISTRATION_TIMEOUT,
            NODE_REGISTRATION_BACKOFF,
        ),
    )
    .unwrap();
    let num_unassigned_nodes = new_topology.unassigned_nodes().count();
    assert_eq!(num_unassigned_nodes, 1);
}

/// Upgrade each HostOS VM to the test version, and verify that each is
/// healthy before and after the upgrade.
pub fn upgrade(env: TestEnv, to: &NestedVersionTarget) {
    let logger = env.logger();

    let (target_version, url, sha256) = match to {
        NestedVersionTarget::Mainnet => (
            env.get_mainnet_version().unwrap(),
            env.get_mainnet_hostos_update_img_url().unwrap(),
            env.get_mainnet_hostos_update_img_sha256().unwrap(),
        ),
        NestedVersionTarget::Branch(false) => (
            env.get_branch_version().unwrap(),
            env.get_hostos_update_img_url().unwrap(),
            env.get_hostos_update_img_sha256().unwrap(),
        ),
        NestedVersionTarget::Branch(true) => (
            format!("{}-test", env.get_branch_version().unwrap()),
            env.get_hostos_update_img_test_url().unwrap(),
            env.get_hostos_update_img_test_sha256().unwrap(),
        ),
        NestedVersionTarget::Published {
            version,
            url,
            sha256,
        } => (version.to_owned(), url.to_owned(), sha256.to_owned()),
    };
    let target_version = HostosVersion::try_from(target_version).unwrap();

    let initial_topology = env.topology_snapshot();
    start_nested_vm(env.clone());
    info!(logger, "Waiting for node to join ...");
    let new_topology = block_on(
        initial_topology.block_for_newer_registry_version_within_duration(
            NODE_REGISTRATION_TIMEOUT,
            NODE_REGISTRATION_BACKOFF,
        ),
    )
    .unwrap();

    let host = env
        .get_nested_vm(HOST_VM_NAME)
        .expect("Unable to find HostOS node.");

    // Check version
    info!(
        logger,
        "Checking version via SSH on HostOS: '{}'",
        host.get_vm().expect("Unable to get HostOS VM.").ipv6
    );
    let original_version = check_hostos_version(&host);
    info!(logger, "Version found is: '{}'", original_version);

    // Add the node to a subnet to start the replica
    let node_id = new_topology.unassigned_nodes().next().unwrap().node_id;

    // Choose a node from the nns subnet
    let nns_subnet = new_topology.root_subnet();
    let nns_node = nns_subnet.nodes().next().unwrap();
    info!(
        logger,
        "Adding node '{}' to subnet '{}'", node_id, nns_subnet.subnet_id
    );
    block_on(add_nodes_to_subnet(
        nns_node.get_public_url(),
        nns_subnet.subnet_id,
        &[node_id],
    ))
    .unwrap();
    host.await_status_is_healthy().unwrap();

    // Elect target HostOS version
    info!(logger, "Electing target HostOS version '{target_version}' with sha256 '{sha256}' and upgrade urls: '{url}'");
    block_on(elect_hostos_version(
        &nns_node,
        &target_version,
        &sha256,
        vec![url.to_string()],
    ));
    info!(logger, "Elected target HostOS version");

    info!(
        logger,
        "Upgrading node '{}' to '{}'", node_id, target_version
    );
    block_on(update_nodes_hostos_version(
        &nns_node,
        &target_version,
        vec![node_id],
    ));

    // The HostOS upgrade is applied with a reboot to the host VM, so we will
    // lose access to the replica. Ensure that it comes back successfully in
    // the new system.
    info!(logger, "Waiting for the upgrade to apply...");
    host.await_status_is_unavailable().unwrap();
    info!(logger, "Waiting for the replica to come back healthy...");
    host.await_status_is_healthy().unwrap();

    // Check the HostOS version again
    info!(
        logger,
        "Checking version via SSH on HostOS: '{}'",
        host.get_vm().expect("Unable to get HostOS VM.").ipv6
    );
    let new_version = check_hostos_version(&host);
    info!(logger, "Version found is: '{}'", new_version);

    assert!(new_version != original_version);
}
