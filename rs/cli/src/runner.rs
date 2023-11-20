use crate::clients::DashboardBackendClient;
use crate::ic_admin;
use crate::ic_admin::ProposeOptions;
use crate::ops_subnet_node_replace;
use decentralization::SubnetChangeResponse;
use ic_base_types::PrincipalId;
use ic_management_types::requests::NodesRemoveRequest;
use ic_management_types::{Artifact, NodeFeature};
use itertools::Itertools;
use log::{info, warn};

#[derive(Clone)]
pub struct Runner {
    ic_admin: ic_admin::IcAdminWrapper,
    dashboard_backend_client: DashboardBackendClient,
}

impl Runner {
    pub fn deploy(&self, subnet: &PrincipalId, version: &str, simulate: bool) -> anyhow::Result<()> {
        self.ic_admin
            .propose_run(
                ic_admin::ProposeCommand::UpdateSubnetReplicaVersion {
                    subnet: *subnet,
                    version: version.to_string(),
                },
                ic_admin::ProposeOptions {
                    title: format!("Update subnet {subnet} to replica version {version}").into(),
                    summary: format!("Update subnet {subnet} to replica version {version}").into(),
                    motivation: None,
                },
                simulate,
            )
            .map_err(|e| anyhow::anyhow!(e))?;

        Ok(())
    }

    pub async fn subnet_resize(
        &self,
        request: ic_management_types::requests::SubnetResizeRequest,
        motivation: String,
        verbose: bool,
        simulate: bool,
    ) -> anyhow::Result<()> {
        let subnet = request.subnet;
        let change = self.dashboard_backend_client.subnet_resize(request).await?;
        if verbose {
            if let Some(run_log) = &change.run_log {
                println!("{}\n", run_log.join("\n"));
            }
        }
        println!("{}", change);

        if change.added.is_empty() && change.removed.is_empty() {
            return Ok(());
        }
        if change.added.len() == change.removed.len() {
            self.run_membership_change(
                change.clone(),
                ops_subnet_node_replace::replace_proposal_options(&change)?,
                simulate,
            )
            .await
        } else {
            let action = if change.added.len() < change.removed.len() {
                "Removing nodes from"
            } else {
                "Adding nodes to"
            };
            self.run_membership_change(
                change,
                ProposeOptions {
                    title: format!("{action} subnet {subnet}").into(),
                    summary: format!("{action} subnet {subnet}").into(),
                    motivation: motivation.clone().into(),
                },
                simulate,
            )
            .await
        }
    }

    pub async fn subnet_create(
        &self,
        request: ic_management_types::requests::SubnetCreateRequest,
        motivation: String,
        verbose: bool,
        simulate: bool,
        replica_version: Option<String>,
    ) -> anyhow::Result<()> {
        let subnet_creation_data = self.dashboard_backend_client.subnet_create(request).await?;
        if verbose {
            if let Some(run_log) = &subnet_creation_data.run_log {
                println!("{}\n", run_log.join("\n"));
            }
        }
        println!("{}", subnet_creation_data);

        let replica_version = replica_version.unwrap_or(
            self.dashboard_backend_client
                .get_nns_replica_version()
                .await
                .expect("Should get a replica version"),
        );

        self.ic_admin.propose_run(
            ic_admin::ProposeCommand::CreateSubnet {
                node_ids: subnet_creation_data.added,
                replica_version,
            },
            ic_admin::ProposeOptions {
                title: Some("Creating new subnet".into()),
                summary: Some("# Creating new subnet with nodes: ".into()),
                motivation: Some(motivation.clone()),
            },
            simulate,
        )
    }

    pub async fn membership_replace(
        &self,
        request: ic_management_types::requests::MembershipReplaceRequest,
        verbose: bool,
        simulate: bool,
    ) -> anyhow::Result<()> {
        let change = self.dashboard_backend_client.membership_replace(request).await?;
        if verbose {
            if let Some(run_log) = &change.run_log {
                println!("{}\n", run_log.join("\n"));
            }
        }
        println!("{}", change);

        if change.added.is_empty() && change.removed.is_empty() {
            return Ok(());
        }
        self.run_membership_change(
            change.clone(),
            ops_subnet_node_replace::replace_proposal_options(&change)?,
            simulate,
        )
        .await
    }

    async fn run_membership_change(
        &self,
        change: SubnetChangeResponse,
        options: ProposeOptions,
        simulate: bool,
    ) -> anyhow::Result<()> {
        let subnet_id = change
            .subnet_id
            .ok_or_else(|| anyhow::anyhow!("subnet_id is required"))?;
        let pending_action = self.dashboard_backend_client.subnet_pending_action(subnet_id).await?;
        if let Some(proposal) = pending_action {
            return Err(anyhow::anyhow!(format!(
                "There is a pending proposal for this subnet: https://dashboard.internetcomputer.org/proposal/{}",
                proposal.id
            )));
        }

        self.ic_admin
            .propose_run(
                ic_admin::ProposeCommand::ChangeSubnetMembership {
                    subnet_id,
                    node_ids_add: change.added.clone(),
                    node_ids_remove: change.removed.clone(),
                },
                options,
                simulate,
            )
            .map_err(|e| anyhow::anyhow!(e))
    }

    pub async fn new_with_network_url(ic_admin: ic_admin::IcAdminWrapper, backend_port: u16) -> anyhow::Result<Self> {
        let dashboard_backend_client =
            DashboardBackendClient::new_with_network_url(format!("http://localhost:{}/", backend_port));
        Ok(Self {
            ic_admin,
            dashboard_backend_client,
        })
    }

    pub(crate) async fn prepare_versions_to_retire(
        &self,
        release_artifact: &Artifact,
        edit_summary: bool,
    ) -> anyhow::Result<(String, Option<Vec<String>>)> {
        let retireable_versions = self
            .dashboard_backend_client
            .get_retireable_versions(release_artifact)
            .await?;

        let versions = if retireable_versions.is_empty() {
            Vec::new()
        } else {
            info!("Waiting for you to pick the versions to retire in your editor");
            let template = "# In the below lines, comment out the versions that you DO NOT want to retire".to_string();
            let versions = edit::edit(format!(
                "{}\n{}",
                template,
                retireable_versions
                    .into_iter()
                    .map(|r| format!("{} # {}", r.commit_hash, r.branch))
                    .join("\n"),
            ))?
            .trim()
            .replace("\r(\n)?", "\n")
            .split('\n')
            .map(|s| regex::Regex::new("#.+$").unwrap().replace_all(s, "").to_string())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

            if versions.is_empty() {
                warn!("Empty list of replica versions to unelect");
            }
            versions
        };

        let mut template =
            "Removing the obsolete IC replica versions from the registry, to prevent unintended version downgrades in the future"
                .to_string();
        if edit_summary {
            info!("Edit summary");
            template = edit::edit(template)?.trim().replace("\r(\n)?", "\n");
        }

        Ok((template, (!versions.is_empty()).then_some(versions)))
    }

    pub async fn remove_nodes(&self, request: NodesRemoveRequest, simulate: bool) -> anyhow::Result<()> {
        let node_remove_response = self.dashboard_backend_client.remove_nodes(request).await?;
        let mut node_removals = node_remove_response.removals;
        node_removals.sort_by_key(|nr| nr.reason.message());

        let headers = vec!["Principal".to_string()]
            .into_iter()
            .chain(NodeFeature::variants().iter().map(|nf| nf.to_string()))
            .chain(vec!["Hostname".to_string()].into_iter())
            .chain(vec!["Reason".to_string()].into_iter())
            .collect::<Vec<_>>();
        let mut table = tabular::Table::new(&headers.iter().map(|_| "    {:<}").collect::<Vec<_>>().join(""));
        // Headers
        let mut header_row = tabular::Row::new();
        for h in headers {
            header_row.add_cell(h);
        }
        table.add_row(header_row);

        // Values
        for nr in &node_removals {
            let mut row = tabular::Row::new();
            let decentralization_node = decentralization::network::Node::from(&nr.node);
            row.add_cell(nr.node.principal);
            for nf in NodeFeature::variants() {
                row.add_cell(decentralization_node.get_feature(&nf));
            }
            row.add_cell(nr.node.hostname.clone().unwrap_or_else(|| "N/A".to_string()));
            row.add_cell(nr.reason.message());
            table.add_row(row);
        }
        println!("{}", table);

        self.ic_admin.propose_run(
            ic_admin::ProposeCommand::RemoveNodes {
                nodes: node_removals.iter().map(|n| n.node.principal).collect(),
            },
            ProposeOptions {
                title: "Remove nodes from the network".to_string().into(),
                summary: "Remove nodes from the network".to_string().into(),
                motivation: node_remove_response.motivation.into(),
            },
            simulate,
        )
    }
}
