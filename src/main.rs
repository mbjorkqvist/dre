#[macro_use]
extern crate diesel;
use clap::Parser;
use decentralization::{OptimizeQuery, SubnetChangeResponse};
use dialoguer::Confirm;
use diesel::prelude::*;
use dotenv::dotenv;
use ic_base_types::PrincipalId;
use log::{debug, error, info, warn};
use mercury_management_types::TopologyProposalStatus;
use tokio::time::{sleep, Duration};
use utils::env_cfg;
mod autoops_types;
mod cli;
mod clients;
mod ic_admin;
mod model_proposals;
mod model_subnet_update_nodes;
mod ops_subnet_node_replace;
mod schema;
mod utils;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    init_env();

    let db_connection = init_sqlite_connect();
    let cli_opts = cli::Opts::parse();
    init_logger();

    ic_admin::with_ic_admin(Default::default(), async {
        let runner = Runner {
            ic_admin: ic_admin::Cli::from(&cli_opts),
            dashboard_backend_client: clients::DashboardBackendClient {
                url: cli_opts.backend_url.clone(),
            },
            decentralization_client: clients::DecenetralizationClient {
                url: cli_opts.decentralization_url.clone(),
            },
        };

        // Start of actually doing stuff with commands.
        match &cli_opts.subcommand {
            cli::Commands::SubnetReplaceNodes { subnet, add, remove } => {
                let ica = ic_admin::CliDeprecated::from(&cli_opts);
                match ops_subnet_node_replace::subnet_nodes_replace(
                    &ica,
                    &db_connection,
                    subnet,
                    add.clone(),
                    remove.clone(),
                ) {
                    Ok(stdout) => {
                        println!("{}", stdout);
                        Ok(())
                    }
                    Err(err) => Err(err),
                }?;
                loop {
                    let pending = ops_subnet_node_replace::check_and_submit_proposals_subnet_add_nodes(
                        &ica,
                        &db_connection,
                        &subnet.to_string(),
                    )?;
                    if pending {
                        info!("There are pending proposals. Waiting 10 seconds");
                        std::thread::sleep(std::time::Duration::from_secs(10));
                    } else {
                        break;
                    }
                }
                info!("There are no more pending proposals. Exiting...");
                Ok(())
            }

            cli::Commands::DerToPrincipal { path } => {
                let principal = ic_base_types::PrincipalId::new_self_authenticating(&std::fs::read(path)?);
                println!("{}", principal);
                Ok(())
            }
            cli::Commands::Subnet(subnet) => match &subnet.subcommand {
                cli::subnet::Commands::Deploy { version } => runner.deploy(&subnet.id, version),
                cli::subnet::Commands::Optimize { max_replacements } => {
                    runner.optimize(subnet.id, *max_replacements).await
                }
            },
            cli::Commands::Node(node) => match &node.subcommand {
                cli::node::Commands::Replace { nodes } => runner.replace(nodes).await,
            },
        }
    })
    .await
}

pub struct Runner {
    ic_admin: ic_admin::Cli,
    dashboard_backend_client: clients::DashboardBackendClient,
    decentralization_client: clients::DecenetralizationClient,
}

impl Runner {
    fn deploy(&self, subnet: &PrincipalId, version: &String) -> anyhow::Result<()> {
        let stdout = self
            .ic_admin
            .propose_run(
                ic_admin::ProposeCommand::UpdateSubnetReplicaVersion {
                    subnet: subnet.clone(),
                    version: version.clone(),
                },
                ic_admin::ProposeOptions {
                    title: format!("Update subnet {subnet} to replica version {version}").into(),
                    summary: format!("Update subnet {subnet} to replica version {version}").into(),
                },
            )
            .map_err(|e| anyhow::anyhow!(e))?;
        info!("{}", stdout);

        Ok(())
    }

    async fn optimize(&self, subnet: PrincipalId, max_replacements: Option<usize>) -> anyhow::Result<()> {
        let change = self
            .decentralization_client
            .optimize(subnet, OptimizeQuery { max_replacements })
            .await?;
        self.swap_nodes(change).await
    }

    async fn replace(&self, nodes: &[PrincipalId]) -> anyhow::Result<()> {
        let change = self.decentralization_client.replace(nodes).await?;
        self.swap_nodes(change).await
    }

    async fn swap_nodes(&self, change: SubnetChangeResponse) -> anyhow::Result<()> {
        if !self.ic_admin.dry_run {
            self.dry().run_swap_nodes(change.clone()).await?;
            if !Confirm::new()
                .with_prompt("Do you want to continue?")
                .default(false)
                .interact()?
            {
                return Err(anyhow::anyhow!("Action aborted"));
            }
        }

        self.run_swap_nodes(change).await
    }

    async fn run_swap_nodes(&self, change: SubnetChangeResponse) -> anyhow::Result<()> {
        let subnet_id = change
            .subnet_id
            .ok_or_else(|| anyhow::anyhow!("subnet_id is required"))?;
        let pending_action = self.dashboard_backend_client.subnet_pending_action(subnet_id).await?;
        if let Some(proposal) = pending_action {
            return Err(anyhow::anyhow!(vec![
                format!(
                    "There is a pending proposal for this subnet: https://dashboard.internetcomputer.org/proposal/{}",
                    proposal.id
                ),
                "Please complete it first by running `release_cli subnet --subnet-id {subnet_id} tidy`".to_string(),
            ]
            .join("\n")));
        }

        self.ic_admin
            .propose_run(
                ic_admin::ProposeCommand::AddNodesToSubnet {
                    subnet_id,
                    nodes: change.added.clone(),
                },
                ops_subnet_node_replace::replace_proposal_options(&change, None)?,
            )
            .map_err(|e| anyhow::anyhow!(e))?;

        let add_proposal_id = if !self.ic_admin.dry_run {
            loop {
                if let Some(proposal) = self.dashboard_backend_client.subnet_pending_action(subnet_id).await? {
                    if matches!(proposal.status, TopologyProposalStatus::Executed) {
                        break proposal.id;
                    }
                }
                sleep(Duration::from_secs(10)).await;
            }
        } else {
            const DUMMY_ID: u64 = 1234567890;
            warn!("Set the first proposal ID to a dummy value: {}", DUMMY_ID);
            DUMMY_ID
        }
        .into();

        self.ic_admin
            .propose_run(
                ic_admin::ProposeCommand::RemoveNodesFromSubnet {
                    nodes: change.removed.clone(),
                },
                ops_subnet_node_replace::replace_proposal_options(&change, add_proposal_id)?,
            )
            .map_err(|e| anyhow::anyhow!(e))?;

        Ok(())
    }

    fn dry(&self) -> Self {
        Self {
            ic_admin: self.ic_admin.dry_run(),
            dashboard_backend_client: self.dashboard_backend_client.clone(),
            decentralization_client: self.decentralization_client.clone(),
        }
    }
}

fn init_sqlite_connect() -> SqliteConnection {
    debug!("Initializing the SQLite connection.");
    let home_path = std::env::var("HOME").expect("Getting HOME environment variable failed.");
    let database_url = env_cfg("DATABASE_URL").replace("~/", format!("{}/", home_path).as_str());
    let database_url_dirname = std::path::Path::new(&database_url)
        .parent()
        .expect("Getting the dirname for the database_url failed.");
    std::fs::create_dir_all(database_url_dirname).expect("Creating the directory for the database file failed.");
    SqliteConnection::establish(&database_url).unwrap_or_else(|_| panic!("Error connecting to {}", database_url))
}

fn init_env() {
    dotenv().expect(".env file not found. Please copy env.template to .env and adjust configuration.");
}

fn init_logger() {
    match std::env::var("RUST_LOG") {
        Ok(val) => std::env::set_var("LOG_LEVEL", val),
        Err(_) => {
            if std::env::var("LOG_LEVEL").is_err() {
                // Set a default logging level: info, if nothing else specified in environment
                // variables RUST_LOG or LOG_LEVEL
                std::env::set_var("LOG_LEVEL", "info")
            }
        }
    }
    pretty_env_logger::init_custom_env("LOG_LEVEL");
}
