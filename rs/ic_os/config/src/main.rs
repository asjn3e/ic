use anyhow::Result;
use clap::{Parser, Subcommand};
use config::config_ini::get_config_ini_settings;
use config::deployment_json::read_deployment_file;
use config::firewall_json::get_firewall_rules_json_or_default;
use config::serialize_and_write_config;
use std::fs::File;
use std::path::{Path, PathBuf};

use config::types::{
    GuestOSSettings, HostOSConfig, HostOSSettings, ICOSSettings, Logging, SetupOSConfig,
    SetupOSSettings,
};

#[derive(Subcommand)]
pub enum Commands {
    /// Creates SetupOSConfig object
    CreateSetuposConfig {
        #[arg(long, default_value = config::DEFAULT_SETUPOS_CONFIG_FILE_PATH, value_name = "config.ini")]
        config_ini_path: PathBuf,

        #[arg(long, default_value = config::DEFAULT_SETUPOS_DEPLOYMENT_JSON_PATH, value_name = "deployment.json")]
        deployment_json_path: PathBuf,

        #[arg(long, default_value = config::DEFAULT_SETUPOS_NNS_PUBLIC_KEY_PATH, value_name = "nns_public_key.pem")]
        nns_public_key_path: PathBuf,

        #[arg(long, default_value = config::DEFAULT_SETUPOS_SSH_AUTHORIZED_KEYS_PATH, value_name = "ssh_authorized_keys")]
        ssh_authorized_keys_path: PathBuf,

        #[arg(long, default_value = config::DEFAULT_SETUPOS_NODE_OPERATOR_PRIVATE_KEY_PATH, value_name = "node_operator_private_key.pem")]
        node_operator_private_key_path: PathBuf,

        #[arg(long, default_value = config::DEFAULT_SETUPOS_CONFIG_OBJECT_PATH, value_name = "config.json")]
        setupos_config_json_path: PathBuf,

        #[arg(long, default_value = None, value_name = "firewall.json")]
        firewall_json_path: Option<PathBuf>,
    },
    /// Creates HostOSConfig object from existing SetupOS config.json file
    GenerateHostosConfig {
        #[arg(long, default_value = config::DEFAULT_SETUPOS_CONFIG_OBJECT_PATH, value_name = "config.json")]
        setupos_config_json_path: PathBuf,
    },
}

#[derive(Parser)]
#[command()]
struct ConfigArgs {
    #[command(subcommand)]
    command: Option<Commands>,
}

pub fn main() -> Result<()> {
    let opts = ConfigArgs::parse();

    match opts.command {
        Some(Commands::CreateSetuposConfig {
            config_ini_path,
            deployment_json_path,
            nns_public_key_path,
            ssh_authorized_keys_path,
            node_operator_private_key_path,
            setupos_config_json_path,
            firewall_json_path,
        }) => {
            // get config.ini variables
            let config_ini_settings = get_config_ini_settings(&config_ini_path)?;
            let mut network_settings = config_ini_settings.network_settings;

            // get deployment.json variables
            let deployment_json = read_deployment_file(&deployment_json_path)?;
            network_settings.mgmt_mac = deployment_json.deployment.mgmt_mac;

            // get firewall.json rules
            let firewall = get_firewall_rules_json_or_default(
                firewall_json_path.as_ref().map(Path::new),
                Path::new(config::DEFAULT_SETUPOS_FIREWALL_JSON_PATH),
            )?;
            network_settings.firewall = firewall;

            let logging = Logging {
                elasticsearch_hosts: deployment_json.logging.hosts.to_string(),
                elasticsearch_tags: None,
            };

            let icos_settings = ICOSSettings {
                logging,
                nns_public_key_path: nns_public_key_path.to_path_buf(),
                nns_urls: deployment_json.nns.url.clone(),
                hostname: deployment_json.deployment.name.to_string(),
                node_operator_private_key_path: node_operator_private_key_path
                    .exists()
                    .then_some(node_operator_private_key_path),
                ssh_authorized_keys_path: ssh_authorized_keys_path
                    .exists()
                    .then_some(ssh_authorized_keys_path),
            };

            let setupos_settings = SetupOSSettings;

            let hostos_settings = HostOSSettings {
                vm_memory: deployment_json.resources.memory,
                vm_cpu: deployment_json
                    .resources
                    .cpu
                    .clone()
                    .unwrap_or("kvm".to_string()),
                verbose: config_ini_settings.verbose,
            };

            let guestos_settings = GuestOSSettings::default();

            let setupos_config = SetupOSConfig {
                network_settings,
                icos_settings,
                setupos_settings,
                hostos_settings,
                guestos_settings,
            };

            let setupos_config_json_path = Path::new(&setupos_config_json_path);
            serialize_and_write_config(setupos_config_json_path, &setupos_config)?;

            println!(
                "SetupOSConfig has been written to {}",
                setupos_config_json_path.display()
            );

            Ok(())
        }
        Some(Commands::GenerateHostosConfig {
            setupos_config_json_path,
        }) => {
            let setupos_config_json_path = Path::new(&setupos_config_json_path);

            let setupos_config: SetupOSConfig =
                serde_json::from_reader(File::open(setupos_config_json_path)?)?;

            let hostos_config = HostOSConfig {
                network_settings: setupos_config.network_settings,
                icos_settings: setupos_config.icos_settings,
                hostos_settings: setupos_config.hostos_settings,
                guestos_settings: setupos_config.guestos_settings,
            };

            let hostos_config_output_path = Path::new("/var/ic/config/config-hostos.json");
            serialize_and_write_config(hostos_config_output_path, &hostos_config)?;

            println!(
                "HostOSConfig has been written to {}",
                hostos_config_output_path.display()
            );

            Ok(())
        }
        None => Ok(()),
    }
}
