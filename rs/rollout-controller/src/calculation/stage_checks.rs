use std::{collections::BTreeMap, time::Duration};

use chrono::{Datelike, Days, NaiveDate, Weekday};
use humantime::format_duration;
use ic_base_types::PrincipalId;
use ic_management_backend::proposal::{SubnetUpdateProposal, UpdateUnassignedNodesProposal};
use ic_management_types::Subnet;
use itertools::Itertools;
use slog::{debug, info, Logger};

use super::{Index, Stage};

#[derive(Debug)]
pub enum SubnetAction {
    Noop {
        subnet_short: String,
    },
    Baking {
        subnet_short: String,
        remaining: Duration,
    },
    PendingProposal {
        subnet_short: String,
        proposal_id: u64,
    },
    PlaceProposal {
        is_unassigned: bool,
        subnet_principal: String,
        version: String,
    },
    WaitForNextWeek {
        subnet_short: String,
    },
}

pub fn check_stages<'a>(
    last_bake_status: &'a BTreeMap<String, f64>,
    subnet_update_proposals: &'a [SubnetUpdateProposal],
    unassigned_node_update_proposals: &'a [UpdateUnassignedNodesProposal],
    index: Index,
    logger: Option<&'a Logger>,
    unassigned_version: &'a String,
    subnets: &'a [Subnet],
    now: NaiveDate,
) -> anyhow::Result<Vec<SubnetAction>> {
    let desired_versions = desired_rollout_release_version(subnets.to_vec(), index.releases);
    for (i, stage) in index.rollout.stages.iter().enumerate() {
        if let Some(logger) = logger {
            info!(logger, "Checking stage {}", i)
        }

        let start_of_release = desired_versions.release.date();
        if stage.wait_for_next_week && !week_passed(start_of_release.date(), now) {
            let actions = stage
                .subnets
                .iter()
                .map(|subnet| SubnetAction::WaitForNextWeek {
                    subnet_short: subnet.to_string(),
                })
                .collect();
            return Ok(actions);
        }

        let stage_actions = check_stage(
            last_bake_status,
            subnet_update_proposals,
            unassigned_node_update_proposals,
            stage,
            logger,
            unassigned_version,
            subnets,
            desired_versions.clone(),
        )?;

        if !stage_actions.iter().all(|a| {
            if let SubnetAction::Noop { subnet_short: _ } = a {
                return true;
            }
            return false;
        }) {
            return Ok(stage_actions);
        }

        if let Some(logger) = logger {
            info!(logger, "Stage {} is completed", i)
        }
    }

    if let Some(logger) = logger {
        info!(
            logger,
            "The current rollout '{}' is completed.", desired_versions.release.rc_name
        );
    }

    Ok(vec![])
}

fn week_passed(release_start: NaiveDate, now: NaiveDate) -> bool {
    let mut counter = release_start.clone();
    counter = counter
        .checked_add_days(Days::new(1))
        .expect("Should be able to add a day");
    while counter <= now {
        if counter.weekday() == Weekday::Mon {
            return true;
        }
        counter = counter
            .checked_add_days(Days::new(1))
            .expect("Should be able to add a day");
    }
    false
}

fn check_stage<'a>(
    last_bake_status: &'a BTreeMap<String, f64>,
    subnet_update_proposals: &'a [SubnetUpdateProposal],
    unassigned_node_update_proposals: &'a [UpdateUnassignedNodesProposal],
    stage: &'a Stage,
    logger: Option<&'a Logger>,
    unassigned_version: &'a String,
    subnets: &'a [Subnet],
    desired_versions: DesiredReleaseVersion,
) -> anyhow::Result<Vec<SubnetAction>> {
    let mut stage_actions = vec![];
    if stage.update_unassigned_nodes {
        // Update unassigned nodes
        if let Some(logger) = logger {
            debug!(logger, "Unassigned nodes stage");
        }

        if *unassigned_version != desired_versions.unassigned_nodes.version {
            match unassigned_node_update_proposals.iter().find(|proposal| {
                if !proposal.info.executed {
                    if let Some(version) = &proposal.payload.replica_version {
                        if *version == desired_versions.unassigned_nodes.version {
                            return true;
                        }
                    }
                }
                return false;
            }) {
                None => stage_actions.push(SubnetAction::PlaceProposal {
                    is_unassigned: true,
                    subnet_principal: "".to_string(),
                    version: desired_versions.unassigned_nodes.version,
                }),
                Some(proposal) => stage_actions.push(SubnetAction::PendingProposal {
                    subnet_short: "unassigned-version".to_string(),
                    proposal_id: proposal.info.id,
                }),
            }
            return Ok(stage_actions);
        }

        stage_actions.push(SubnetAction::Noop {
            subnet_short: "unassigned-nodes".to_string(),
        });
        return Ok(stage_actions);
    }

    for subnet_short in &stage.subnets {
        // Get desired version
        let (subnet_principal, desired_version) = desired_versions
            .subnets
            .iter()
            .find(|(s, _)| s.to_string().starts_with(subnet_short))
            .expect("should find the subnet");

        // Find subnet to by the subnet_short
        let subnet = subnets
            .iter()
            .find(|s| *subnet_principal == s.principal)
            .expect("subnet should exist");

        if let Some(logger) = logger {
            debug!(
                logger,
                "Checking if subnet {} is on desired version '{}'", subnet_short, desired_version.version
            );
        }

        // If subnet is on desired version, check bake time
        if *subnet.replica_version == desired_version.version {
            let remaining =
                get_remaining_bake_time_for_subnet(last_bake_status, subnet, stage.bake_time.as_secs_f64())?;
            let remaining_duration = Duration::from_secs_f64(remaining);
            let formatted = format_duration(remaining_duration);

            if remaining != 0.0 {
                stage_actions.push(SubnetAction::Baking {
                    subnet_short: subnet_short.clone(),
                    remaining: remaining_duration,
                });
                continue;
            }

            if let Some(logger) = logger {
                if remaining == 0.0 {
                    debug!(logger, "Subnet {} baked", subnet_short)
                } else {
                    debug!(
                        logger,
                        "Waiting for subnet {} to bake, remaining {}", subnet_short, formatted
                    )
                }
            }

            stage_actions.push(SubnetAction::Noop {
                subnet_short: subnet_short.clone(),
            });
            continue;
        }

        // If subnet is not on desired version, check if there is an open proposal
        if let Some(proposal) = get_open_proposal_for_subnet(subnet_update_proposals, subnet, &desired_version.version)
        {
            if let Some(logger) = logger {
                info!(
                    logger,
                    "For subnet '{}' found open proposal with id '{}'", subnet_short, proposal.info.id
                )
            }
            stage_actions.push(SubnetAction::PendingProposal {
                subnet_short: subnet_short.clone(),
                proposal_id: proposal.info.id,
            });
            continue;
        }

        // If subnet is not on desired version and there is no open proposal submit it
        stage_actions.push(SubnetAction::PlaceProposal {
            is_unassigned: false,
            subnet_principal: subnet.principal.to_string(),
            version: desired_version.version.clone(),
        })
    }

    Ok(stage_actions)
}

#[derive(Clone)]
struct DesiredReleaseVersion {
    subnets: BTreeMap<PrincipalId, crate::calculation::Version>,
    unassigned_nodes: crate::calculation::Version,
    release: crate::calculation::Release,
}

fn desired_rollout_release_version(
    subnets: Vec<Subnet>,
    releases: Vec<crate::calculation::Release>,
) -> DesiredReleaseVersion {
    let subnets_releases = subnets
        .iter()
        .map(|s| {
            releases
                .iter()
                .find(|r| r.versions.iter().any(|v| v.version == s.replica_version))
                .expect("version should exist in releases")
        })
        .unique()
        .collect::<Vec<_>>();
    // assumes `releases` are already sorted, but we can sort it if needed
    if subnets_releases.len() > 2 {
        panic!("more than two releases active")
    }
    let mut newest_release = releases
        .iter()
        .find(|r| subnets_releases.contains(r))
        .expect("should find some release");

    if subnets_releases.len() == 1 {
        newest_release = &releases[releases
            .iter()
            .position(|r| r == newest_release)
            .expect("release should exist")
            .saturating_sub(1)];
    }
    DesiredReleaseVersion {
        release: newest_release.clone(),
        subnets: subnets
        .iter()
        .map(|s| {
            (
                s.principal,
                newest_release
                    .versions
                    .iter()
                    .find_or_first(|v| v.subnets.iter().any(|vs| s.principal.to_string().starts_with(vs)))
                    .expect("versions should not be empty so it should return the first element if it doesn't match anything").clone(),
            )
        })
        .collect(),
         unassigned_nodes: newest_release.versions[0].clone(),
    }
}

fn get_remaining_bake_time_for_subnet(
    last_bake_status: &BTreeMap<String, f64>,
    subnet: &Subnet,
    stage_bake_time: f64,
) -> anyhow::Result<f64> {
    let bake = match last_bake_status.get(&subnet.principal.to_string()) {
        Some(bake) => bake,
        None => {
            return Err(anyhow::anyhow!(
                "Subnet with principal '{}' not found",
                subnet.principal.to_string()
            ))
        }
    };

    return match bake.ge(&stage_bake_time) {
        true => Ok(0.0),
        false => {
            let remaining = Duration::from_secs_f64(stage_bake_time - bake);
            return Ok(remaining.as_secs_f64());
        }
    };
}

fn get_open_proposal_for_subnet<'a>(
    subnet_update_proposals: &'a [SubnetUpdateProposal],
    subnet: &'a Subnet,
    desired_version: &'a str,
) -> Option<&'a SubnetUpdateProposal> {
    subnet_update_proposals.iter().find(|p| {
        p.payload.subnet_id == subnet.principal && p.payload.replica_version_id.eq(desired_version) && !p.info.executed
    })
}

#[cfg(test)]
mod week_passed_tests {
    use super::*;
    use chrono::NaiveDate;
    use rstest::rstest;

    #[rstest]
    #[case("2024-03-13", "2024-03-18", true)]
    #[case("2024-03-13", "2024-03-19", true)]
    #[case("2024-03-03", "2024-03-19", true)]
    #[case("2024-03-13", "2024-03-13", false)]
    #[case("2024-03-13", "2024-03-15", false)]
    #[case("2024-03-13", "2024-03-17", false)]
    fn should_complete(#[case] release_start: &str, #[case] now: &str, #[case] outcome: bool) {
        let release_start = NaiveDate::parse_from_str(release_start, "%Y-%m-%d").expect("Should be able to parse date");
        let now = NaiveDate::parse_from_str(now, "%Y-%m-%d").expect("Should be able to parse date");

        assert_eq!(week_passed(release_start, now), outcome)
    }
}

#[cfg(test)]
mod get_open_proposal_for_subnet_tests {
    use std::str::FromStr;

    use candid::Principal;
    use ic_base_types::PrincipalId;
    use ic_management_backend::proposal::ProposalInfoInternal;
    use registry_canister::mutations::do_update_subnet_replica::UpdateSubnetReplicaVersionPayload;

    use super::*;
    use rstest::rstest;

    pub(super) fn craft_subnet_from_id<'a>(subnet_id: &'a str) -> Subnet {
        Subnet {
            principal: PrincipalId(Principal::from_str(subnet_id).expect("Can create principal")),
            ..Default::default()
        }
    }

    pub(super) fn craft_proposals<'a>(
        subnet_with_execution_status: &'a [(&'a str, bool)],
        version: &'a str,
    ) -> impl Iterator<Item = SubnetUpdateProposal> + 'a {
        subnet_with_execution_status
            .iter()
            .enumerate()
            .map(|(i, (id, executed))| SubnetUpdateProposal {
                payload: UpdateSubnetReplicaVersionPayload {
                    subnet_id: PrincipalId(Principal::from_str(id).expect("Can create principal")),
                    replica_version_id: version.to_string(),
                },
                info: ProposalInfoInternal {
                    id: i as u64,
                    // These values are not important for the function
                    executed_timestamp_seconds: 1,
                    proposal_timestamp_seconds: 1,
                    executed: *executed,
                },
            })
    }

    pub(super) fn craft_open_proposals<'a>(subnet_ids: &'a [&'a str], version: &'a str) -> Vec<SubnetUpdateProposal> {
        craft_proposals(
            &subnet_ids.iter().map(|id| (*id, false)).collect::<Vec<(&str, bool)>>(),
            version,
        )
        .collect()
    }

    pub(super) fn craft_executed_proposals<'a>(
        subnet_ids: &'a [&'a str],
        version: &'a str,
    ) -> Vec<SubnetUpdateProposal> {
        craft_proposals(
            &subnet_ids.iter().map(|id| (*id, true)).collect::<Vec<(&str, bool)>>(),
            version,
        )
        .collect()
    }

    #[test]
    fn should_find_open_proposal_for_subnet() {
        let proposals = craft_open_proposals(
            &vec![
                "snjp4-xlbw4-mnbog-ddwy6-6ckfd-2w5a2-eipqo-7l436-pxqkh-l6fuv-vae",
                "pae4o-o6dxf-xki7q-ezclx-znyd6-fnk6w-vkv5z-5lfwh-xym2i-otrrw-fqe",
            ],
            "version",
        );

        let subnet = craft_subnet_from_id("snjp4-xlbw4-mnbog-ddwy6-6ckfd-2w5a2-eipqo-7l436-pxqkh-l6fuv-vae");
        let proposal = get_open_proposal_for_subnet(&proposals, &subnet, "version");

        assert!(proposal.is_some())
    }

    #[rstest]
    #[case(
        "version",
        "snjp4-xlbw4-mnbog-ddwy6-6ckfd-2w5a2-eipqo-7l436-pxqkh-l6fuv-vae",
        "version"
    )]
    #[case(
        "other-version",
        "snjp4-xlbw4-mnbog-ddwy6-6ckfd-2w5a2-eipqo-7l436-pxqkh-l6fuv-vae",
        "version"
    )]
    #[case(
        "version",
        "5kdm2-62fc6-fwnja-hutkz-ycsnm-4z33i-woh43-4cenu-ev7mi-gii6t-4ae",
        "version"
    )]
    fn should_not_find_open_proposal(
        #[case] proposal_version: &str,
        #[case] subnet_id: &str,
        #[case] current_version: &str,
    ) {
        let proposals = craft_executed_proposals(
            &vec![
                "snjp4-xlbw4-mnbog-ddwy6-6ckfd-2w5a2-eipqo-7l436-pxqkh-l6fuv-vae",
                "pae4o-o6dxf-xki7q-ezclx-znyd6-fnk6w-vkv5z-5lfwh-xym2i-otrrw-fqe",
            ],
            proposal_version,
        );
        let subnet = craft_subnet_from_id(subnet_id);
        let proposal = get_open_proposal_for_subnet(&proposals, &subnet, current_version);

        assert!(proposal.is_none())
    }
}

#[cfg(test)]
mod get_remaining_bake_time_for_subnet_tests {
    use super::*;
    use rstest::rstest;

    fn craft_bake_status_from_tuples(tuples: &[(&str, f64)]) -> BTreeMap<String, f64> {
        tuples
            .iter()
            .map(|(id, bake_time)| (id.to_string(), *bake_time))
            .collect::<BTreeMap<String, f64>>()
    }

    #[test]
    fn should_return_error_subnet_not_found() {
        let subnet = get_open_proposal_for_subnet_tests::craft_subnet_from_id(
            "pae4o-o6dxf-xki7q-ezclx-znyd6-fnk6w-vkv5z-5lfwh-xym2i-otrrw-fqe",
        );

        let bake_status = craft_bake_status_from_tuples(&[("random-subnet", 1.0)]);

        let maybe_remaining_bake_time = get_remaining_bake_time_for_subnet(&bake_status, &subnet, 100.0);

        assert!(maybe_remaining_bake_time.is_err())
    }

    #[rstest]
    #[case(100.0, 100.0, 0.0)]
    #[case(150.0, 100.0, 0.0)]
    #[case(100.0, 150.0, 50.0)]
    // Should these be allowed? Technically we will never get
    // something like this from prometheus and there should
    // be validation for incoming configuration, but it is a
    // possibility in our code. Maybe we could add validation
    // checks that disallow of negative baking time?
    #[case(-100.0, 150.0, 250.0)]
    #[case(-100.0, -150.0, 0.0)]
    #[case(-100.0, -50.0, 50.0)]
    fn should_return_subnet_baking_time(
        #[case] subnet_bake_status: f64,
        #[case] stage_bake: f64,
        #[case] remaining: f64,
    ) {
        let subnet = get_open_proposal_for_subnet_tests::craft_subnet_from_id(
            "pae4o-o6dxf-xki7q-ezclx-znyd6-fnk6w-vkv5z-5lfwh-xym2i-otrrw-fqe",
        );

        let bake_status = craft_bake_status_from_tuples(&[(
            "pae4o-o6dxf-xki7q-ezclx-znyd6-fnk6w-vkv5z-5lfwh-xym2i-otrrw-fqe",
            subnet_bake_status,
        )]);

        let maybe_remaining_bake_time = get_remaining_bake_time_for_subnet(&bake_status, &subnet, stage_bake);

        assert!(maybe_remaining_bake_time.is_ok());
        let remaining_bake_time = maybe_remaining_bake_time.unwrap();
        assert_eq!(remaining_bake_time, remaining)
    }
}

#[cfg(test)]
mod test {

    use ic_base_types::PrincipalId;
    use ic_management_types::SubnetMetadata;
    use pretty_assertions::assert_eq;

    use crate::calculation::{Release, Version};

    use super::*;

    #[test]
    fn desired_version_test_cases() {
        struct TestCase {
            name: &'static str,
            subnets: Vec<Subnet>,
            releases: Vec<Release>,
            want: BTreeMap<u64, String>,
        }

        fn subnet(id: u64, version: &str) -> Subnet {
            Subnet {
                principal: PrincipalId::new_subnet_test_id(id),
                replica_version: version.to_string(),
                metadata: SubnetMetadata {
                    name: format!("{id}"),
                    ..Default::default()
                },
                ..Default::default()
            }
        }

        fn release(name: &str, versions: Vec<(&str, Vec<u64>)>) -> Release {
            Release {
                rc_name: name.to_string(),
                versions: versions
                    .iter()
                    .map(|(v, subnets)| Version {
                        version: v.to_string(),
                        subnets: subnets
                            .iter()
                            .map(|id| PrincipalId::new_subnet_test_id(*id).to_string())
                            .collect(),
                        ..Default::default()
                    })
                    .collect(),
            }
        }

        for tc in vec![
            TestCase {
                name: "all versions on the newest version already",
                subnets: vec![subnet(1, "A.default")],
                releases: vec![release("A", vec![("A.default", vec![])])],
                want: vec![(1, "A.default")]
                    .into_iter()
                    .map(|(k, v)| (k, v.to_string()))
                    .collect(),
            },
            TestCase {
                name: "upgrade one subnet",
                subnets: vec![subnet(1, "B.default"), subnet(2, "A.default")],
                releases: vec![
                    release("B", vec![("B.default", vec![])]),
                    release("A", vec![("A.default", vec![])]),
                ],
                want: vec![(1, "B.default"), (2, "B.default")]
                    .into_iter()
                    .map(|(k, v)| (k, v.to_string()))
                    .collect(),
            },
            TestCase {
                name: "extra new and old releases are ignored",
                subnets: vec![subnet(1, "C.default"), subnet(2, "B.default")],
                releases: vec![
                    release("D", vec![("D.default", vec![])]),
                    release("C", vec![("C.default", vec![])]),
                    release("B", vec![("B.default", vec![])]),
                    release("A", vec![("A.default", vec![])]),
                ],
                want: vec![(1, "C.default"), (2, "C.default")]
                    .into_iter()
                    .map(|(k, v)| (k, v.to_string()))
                    .collect(),
            },
            TestCase {
                name: "all subnets on same release, should proceed to upgrade everything to newer release",
                subnets: vec![subnet(1, "B.default"), subnet(2, "B.default")],
                releases: vec![
                    release("D", vec![("D.default", vec![])]),
                    release("C", vec![("C.default", vec![]), ("C.feature", vec![2])]),
                    release("B", vec![("B.default", vec![])]),
                    release("A", vec![("A.default", vec![])]),
                ],
                want: vec![(1, "C.default"), (2, "C.feature")]
                    .into_iter()
                    .map(|(k, v)| (k, v.to_string()))
                    .collect(),
            },
            TestCase {
                name: "feature",
                subnets: vec![subnet(1, "B.default"), subnet(2, "A.default"), subnet(3, "A.default")],
                releases: vec![
                    release("B", vec![("B.default", vec![]), ("B.feature", vec![2])]),
                    release("A", vec![("A.default", vec![])]),
                ],
                want: vec![(1, "B.default"), (2, "B.feature"), (3, "B.default")]
                    .into_iter()
                    .map(|(k, v)| (k, v.to_string()))
                    .collect(),
            },
        ] {
            let desired_release = desired_rollout_release_version(tc.subnets, tc.releases);
            assert_eq!(
                tc.want
                    .into_iter()
                    .map(|(k, v)| (PrincipalId::new_subnet_test_id(k), v))
                    .collect::<Vec<_>>(),
                desired_release
                    .subnets
                    .into_iter()
                    .map(|(k, v)| (k, v.version))
                    .collect::<Vec<_>>(),
                "test case '{}' failed",
                tc.name,
            )
        }
    }
}

// E2E tests for decision making process for happy path without feature builds
#[cfg(test)]
mod check_stages_tests_no_feature_builds {
    use std::str::FromStr;

    use candid::Principal;
    use check_stages_tests_no_feature_builds::get_open_proposal_for_subnet_tests::craft_executed_proposals;
    use ic_base_types::PrincipalId;
    use ic_management_backend::proposal::ProposalInfoInternal;
    use registry_canister::mutations::{
        do_update_subnet_replica::UpdateSubnetReplicaVersionPayload,
        do_update_unassigned_nodes_config::UpdateUnassignedNodesConfigPayload,
    };

    use crate::calculation::{Index, Release, Rollout, Version};

    use super::*;

    /// Part one => No feature builds
    /// `last_bake_status` - can be defined
    /// `subnet_update_proposals` - can be defined
    /// `unassigned_node_update_proposals` - can be defined
    /// `index` - must be defined
    /// `logger` - can be defined, but won't be because these are only tests
    /// `unassigned_version` - should be defined
    /// `subnets` - should be defined
    /// `now` - should be defined
    ///
    /// For all use cases we will use the following setup
    /// rollout:
    ///     pause: false // Tested in `should_proceed.rs` module
    ///     skip_days: [] // Tested in `should_proceed.rs` module
    ///     stages:
    ///         - subnets: [io67a]
    ///           bake_time: 8h
    ///         - subnets: [shefu, uzr34]
    ///           bake_time: 4h
    ///         - update_unassigned_nodes: true
    ///         - subnets: [pjljw]
    ///           wait_for_next_week: true
    ///           bake_time: 4h
    /// releases:
    ///     - rc_name: rc--2024-02-21_23-01
    ///       versions:
    ///         - version: 2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f
    ///           name: rc--2024-02-21_23-01
    ///           release_notes_ready: <not-important>
    ///           subnets: [] // empty because its a regular build
    ///     - rc_name: rc--2024-02-14_23-01
    ///       versions:
    ///         - version: 85bd56a70e55b2cea75cae6405ae11243e5fdad8
    ///           name: rc--2024-02-14_23-01
    ///           release_notes_ready: <not-important>
    ///           subnets: [] // empty because its a regular build
    fn craft_index_state() -> Index {
        Index {
            rollout: Rollout {
                pause: false,
                skip_days: vec![],
                stages: vec![
                    Stage {
                        subnets: vec!["io67a".to_string()],
                        bake_time: humantime::parse_duration("8h").expect("Should be able to parse."),
                        ..Default::default()
                    },
                    Stage {
                        subnets: vec!["shefu".to_string(), "uzr34".to_string()],
                        bake_time: humantime::parse_duration("4h").expect("Should be able to parse."),
                        ..Default::default()
                    },
                    Stage {
                        update_unassigned_nodes: true,
                        ..Default::default()
                    },
                    Stage {
                        subnets: vec!["pjljw".to_string()],
                        bake_time: humantime::parse_duration("4h").expect("Should be able to parse."),
                        wait_for_next_week: true,
                        ..Default::default()
                    },
                ],
            },
            releases: vec![
                Release {
                    rc_name: "rc--2024-02-21_23-01".to_string(),
                    versions: vec![Version {
                        name: "rc--2024-02-21_23-01".to_string(),
                        version: "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string(),
                        ..Default::default()
                    }],
                },
                Release {
                    rc_name: "rc--2024-02-14_23-01".to_string(),
                    versions: vec![Version {
                        name: "rc--2024-02-14_23-01".to_string(),
                        version: "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string(),
                        ..Default::default()
                    }],
                },
            ],
        }
    }

    pub(super) fn craft_subnets() -> Vec<Subnet> {
        vec![
            Subnet {
                principal: PrincipalId(
                    Principal::from_str("io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe")
                        .expect("Should be able to create a principal"),
                ),
                replica_version: "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string(),
                ..Default::default()
            },
            Subnet {
                principal: PrincipalId(
                    Principal::from_str("shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe")
                        .expect("Should be able to create a principal"),
                ),
                replica_version: "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string(),
                ..Default::default()
            },
            Subnet {
                principal: PrincipalId(
                    Principal::from_str("uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe")
                        .expect("Should be able to create a principal"),
                ),
                replica_version: "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string(),
                ..Default::default()
            },
            Subnet {
                principal: PrincipalId(
                    Principal::from_str("pjljw-kztyl-46ud4-ofrj6-nzkhm-3n4nt-wi3jt-ypmav-ijqkt-gjf66-uae")
                        .expect("Should be able to create a principal"),
                ),
                replica_version: "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string(),
                ..Default::default()
            },
        ]
    }

    pub(super) fn replace_versions(subnets: &mut Vec<Subnet>, tuples: &[(&str, &str)]) {
        for (id, ver) in tuples {
            if let Some(subnet) = subnets.iter_mut().find(|s| s.principal.to_string().contains(id)) {
                subnet.replica_version = ver.to_string();
            }
        }
    }

    /// Use-Case 1: Beginning of a new rollout
    ///
    /// `last_bake_status` - empty, because no subnets have the version
    /// `subnet_update_proposals` - can be empty but doesn't have to be. For e.g. if its Monday it is possible to have an open proposal for NNS
    ///                             But it is for a different version (one from last week)
    /// `unassigned_nodes_proposals` - empty
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-21`
    #[test]
    fn test_use_case_1() {
        let index = craft_index_state();
        let current_version = "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string();
        let last_bake_status = BTreeMap::new();
        let subnet_update_proposals = Vec::new();
        let unassigned_version = "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string();
        let unassigned_nodes_proposals = vec![];
        let subnets = &craft_subnets();
        let now = NaiveDate::parse_from_str("2024-02-21", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposals,
            index,
            None,
            &unassigned_version,
            subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();

        assert_eq!(actions.len(), 1);
        for action in actions {
            match action {
                SubnetAction::PlaceProposal {
                    is_unassigned,
                    subnet_principal,
                    version,
                } => {
                    assert_eq!(is_unassigned, false);
                    assert_eq!(version, current_version);
                    assert!(subnet_principal.starts_with("io67a"))
                }
                // Fail the test
                _ => assert!(false),
            }
        }
    }

    /// Use case 2: First batch is submitted but the proposal wasn't executed
    ///
    /// `last_bake_status` - empty, because no subnets have the version
    /// `subnet_update_proposals` - contains proposals from the first stage
    /// `unassigned_nodes_proposals` - empty
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-21`
    #[test]
    fn test_use_case_2() {
        let index = craft_index_state();
        let last_bake_status = BTreeMap::new();
        let subnet_principal = Principal::from_str("io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe")
            .expect("Should be possible to create principal");
        let subnet_update_proposals = vec![SubnetUpdateProposal {
            info: ProposalInfoInternal {
                executed: false,
                executed_timestamp_seconds: 0,
                proposal_timestamp_seconds: 0,
                id: 1,
            },
            payload: UpdateSubnetReplicaVersionPayload {
                subnet_id: PrincipalId(subnet_principal.clone()),
                replica_version_id: "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string(),
            },
        }];
        let unassigned_version = "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string();
        let unassigned_nodes_proposals = vec![];
        let subnets = &craft_subnets();
        let now = NaiveDate::parse_from_str("2024-02-21", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposals,
            index,
            None,
            &unassigned_version,
            subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();
        println!("{:#?}", actions);
        assert_eq!(actions.len(), 1);
        for action in actions {
            match action {
                SubnetAction::PendingProposal {
                    subnet_short,
                    proposal_id,
                } => {
                    assert_eq!(proposal_id, 1);
                    assert!(subnet_principal.to_string().starts_with(&subnet_short))
                }
                // Just fail
                _ => assert!(false),
            }
        }
    }

    /// Use case 3: First batch is submitted the proposal was executed and the subnet is baking
    ///
    /// `last_bake_status` - contains the status for the first subnet
    /// `subnet_update_proposals` - contains proposals from the first stage
    /// `unassigned_nodes_proposals` - empty
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-21`
    #[test]
    fn test_use_case_3() {
        let index = craft_index_state();
        let last_bake_status = [(
            "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
            humantime::parse_duration("3h"),
        )]
        .iter()
        .map(|(id, duration)| {
            (
                id.to_string(),
                duration.clone().expect("Should parse duration").as_secs_f64(),
            )
        })
        .collect();
        let subnet_principal = Principal::from_str("io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe")
            .expect("Should be possible to create principal");
        let subnet_update_proposals = vec![SubnetUpdateProposal {
            info: ProposalInfoInternal {
                executed: true,
                executed_timestamp_seconds: 0,
                proposal_timestamp_seconds: 0,
                id: 1,
            },
            payload: UpdateSubnetReplicaVersionPayload {
                subnet_id: PrincipalId(subnet_principal.clone()),
                replica_version_id: "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string(),
            },
        }];
        let unassigned_version = "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string();
        let unassigned_nodes_proposals = vec![];
        let mut subnets = craft_subnets();
        replace_versions(&mut subnets, &[("io67a", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f")]);
        let now = NaiveDate::parse_from_str("2024-02-21", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposals,
            index,
            None,
            &unassigned_version,
            &subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();
        println!("{:#?}", actions);
        assert_eq!(actions.len(), 1);
        for action in actions {
            match action {
                SubnetAction::Baking {
                    subnet_short,
                    remaining,
                } => {
                    assert!(subnet_principal.to_string().starts_with(&subnet_short));
                    assert!(remaining.eq(&humantime::parse_duration("5h").expect("Should parse duration")))
                }
                // Just fail
                _ => assert!(false),
            }
        }
    }

    /// Use case 4: First batch is submitted the proposal was executed and the subnet is baked, placing proposal for next stage
    ///
    /// `last_bake_status` - contains the status for the first subnet
    /// `subnet_update_proposals` - contains proposals from the first stage
    /// `unassigned_nodes_proposals` - empty
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-21`
    #[test]
    fn test_use_case_4() {
        let index = craft_index_state();
        let current_version = "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string();
        let last_bake_status = [(
            "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
            humantime::parse_duration("9h"),
        )]
        .iter()
        .map(|(id, duration)| {
            (
                id.to_string(),
                duration.clone().expect("Should parse duration").as_secs_f64(),
            )
        })
        .collect();
        let subnet_principal = Principal::from_str("io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe")
            .expect("Should be possible to create principal");
        let subnet_update_proposals = vec![SubnetUpdateProposal {
            info: ProposalInfoInternal {
                executed: true,
                executed_timestamp_seconds: 0,
                proposal_timestamp_seconds: 0,
                id: 1,
            },
            payload: UpdateSubnetReplicaVersionPayload {
                subnet_id: PrincipalId(subnet_principal.clone()),
                replica_version_id: "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string(),
            },
        }];
        let unassigned_version = "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string();
        let unassigned_nodes_proposals = vec![];
        let mut subnets = craft_subnets();
        replace_versions(&mut subnets, &[("io67a", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f")]);
        let now = NaiveDate::parse_from_str("2024-02-21", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposals,
            index,
            None,
            &unassigned_version,
            &subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();
        println!("{:#?}", actions);
        assert_eq!(actions.len(), 2);
        let subnets = vec![
            "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
            "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
        ];
        for action in actions {
            match action {
                SubnetAction::PlaceProposal {
                    is_unassigned,
                    subnet_principal,
                    version,
                } => {
                    assert_eq!(is_unassigned, false);
                    assert_eq!(version, current_version);
                    assert!(subnets.contains(&subnet_principal.as_str()))
                }
                // Just fail
                _ => assert!(false),
            }
        }
    }

    /// Use case 5: Updating unassigned nodes
    ///
    /// `last_bake_status` - contains the status for all subnets before unassigned nodes
    /// `subnet_update_proposals` - contains proposals from previous two stages
    /// `unassigned_nodes_proposals` - empty
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-21`
    #[test]
    fn test_use_case_5() {
        let index = craft_index_state();
        let current_version = "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string();
        let last_bake_status = [
            (
                "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
                humantime::parse_duration("9h"),
            ),
            (
                "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
                humantime::parse_duration("5h"),
            ),
            (
                "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
                humantime::parse_duration("5h"),
            ),
        ]
        .iter()
        .map(|(id, duration)| {
            (
                id.to_string(),
                duration.clone().expect("Should parse duration").as_secs_f64(),
            )
        })
        .collect();
        let subnet_update_proposals = craft_executed_proposals(
            &[
                "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
                "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
                "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
            ],
            &current_version,
        );
        let unassigned_version = "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string();
        let unassigned_nodes_proposals = vec![];
        let mut subnets = craft_subnets();
        replace_versions(
            &mut subnets,
            &[
                ("io67a", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("shefu", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("uzr34", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
            ],
        );
        let now = NaiveDate::parse_from_str("2024-02-21", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposals,
            index,
            None,
            &unassigned_version,
            &subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();
        println!("{:#?}", actions);
        assert_eq!(actions.len(), 1);
        for action in actions {
            match action {
                SubnetAction::PlaceProposal {
                    is_unassigned,
                    subnet_principal: _,
                    version,
                } => {
                    assert!(is_unassigned);
                    assert_eq!(version, current_version);
                }
                // Just fail
                _ => assert!(false),
            }
        }
    }

    /// Use case 6: Proposal sent for updating unassigned nodes but it is not executed
    ///
    /// `last_bake_status` - contains the status for all subnets before unassigned nodes
    /// `subnet_update_proposals` - contains proposals from previous two stages
    /// `unassigned_nodes_proposals` - contains open proposal for unassigned nodes
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-21`
    #[test]
    fn test_use_case_6() {
        let index = craft_index_state();
        let current_version = "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string();
        let last_bake_status = [
            (
                "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
                humantime::parse_duration("9h"),
            ),
            (
                "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
                humantime::parse_duration("5h"),
            ),
            (
                "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
                humantime::parse_duration("5h"),
            ),
        ]
        .iter()
        .map(|(id, duration)| {
            (
                id.to_string(),
                duration.clone().expect("Should parse duration").as_secs_f64(),
            )
        })
        .collect();
        let subnet_update_proposals = craft_executed_proposals(
            &[
                "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
                "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
                "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
            ],
            &current_version,
        );
        let unassigned_version = "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string();
        let unassigned_nodes_proposal = vec![UpdateUnassignedNodesProposal {
            info: ProposalInfoInternal {
                executed: false,
                executed_timestamp_seconds: 0,
                id: 5,
                proposal_timestamp_seconds: 0,
            },
            payload: UpdateUnassignedNodesConfigPayload {
                ssh_readonly_access: None,
                replica_version: Some(current_version.clone()),
            },
        }];
        let mut subnets = craft_subnets();
        replace_versions(
            &mut subnets,
            &[
                ("io67a", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("shefu", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("uzr34", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
            ],
        );
        let now = NaiveDate::parse_from_str("2024-02-21", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposal,
            index,
            None,
            &unassigned_version,
            &subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();
        println!("{:#?}", actions);
        assert_eq!(actions.len(), 1);
        for action in actions {
            match action {
                SubnetAction::PendingProposal {
                    proposal_id,
                    subnet_short,
                } => {
                    assert_eq!(proposal_id, 5);
                    assert_eq!(subnet_short, "unassigned-version");
                }
                // Just fail
                _ => assert!(false),
            }
        }
    }

    /// Use case 7: Executed update unassigned nodes, waiting for next week
    ///
    /// `last_bake_status` - contains the status for all subnets before unassigned nodes
    /// `subnet_update_proposals` - contains proposals from previous two stages
    /// `unassigned_nodes_proposals` - contains executed proposal for unassigned nodes
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-24`
    #[test]
    fn test_use_case_7() {
        let index = craft_index_state();
        let current_version = "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string();
        let last_bake_status = [
            (
                "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
                humantime::parse_duration("9h"),
            ),
            (
                "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
                humantime::parse_duration("5h"),
            ),
            (
                "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
                humantime::parse_duration("5h"),
            ),
        ]
        .iter()
        .map(|(id, duration)| {
            (
                id.to_string(),
                duration.clone().expect("Should parse duration").as_secs_f64(),
            )
        })
        .collect();
        let subnet_update_proposals = craft_executed_proposals(
            &[
                "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
                "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
                "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
            ],
            &current_version,
        );
        let unassigned_version = current_version.clone();
        let unassigned_nodes_proposal = vec![UpdateUnassignedNodesProposal {
            info: ProposalInfoInternal {
                executed: true,
                executed_timestamp_seconds: 0,
                id: 5,
                proposal_timestamp_seconds: 0,
            },
            payload: UpdateUnassignedNodesConfigPayload {
                ssh_readonly_access: None,
                replica_version: Some(current_version.clone()),
            },
        }];
        let mut subnets = craft_subnets();
        replace_versions(
            &mut subnets,
            &[
                ("io67a", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("shefu", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("uzr34", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
            ],
        );
        let now = NaiveDate::parse_from_str("2024-02-24", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposal,
            index,
            None,
            &unassigned_version,
            &subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();
        println!("{:#?}", actions);
        assert_eq!(actions.len(), 1);
        for action in actions {
            match action {
                SubnetAction::WaitForNextWeek { subnet_short } => {
                    assert_eq!(subnet_short, "pjljw");
                }
                // Just fail
                _ => assert!(false),
            }
        }
    }

    /// Use case 8: Next monday came, should place proposal for updating the last subnet
    ///
    /// `last_bake_status` - contains the status for all subnets before unassigned nodes
    /// `subnet_update_proposals` - contains proposals from previous two stages
    /// `unassigned_nodes_proposals` - contains executed proposal for unassigned nodes
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-26`
    #[test]
    fn test_use_case_8() {
        let index = craft_index_state();
        let current_version = "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string();
        let last_bake_status = [
            (
                "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
                humantime::parse_duration("9h"),
            ),
            (
                "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
                humantime::parse_duration("5h"),
            ),
            (
                "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
                humantime::parse_duration("5h"),
            ),
        ]
        .iter()
        .map(|(id, duration)| {
            (
                id.to_string(),
                duration.clone().expect("Should parse duration").as_secs_f64(),
            )
        })
        .collect();
        let subnet_update_proposals = craft_executed_proposals(
            &[
                "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
                "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
                "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
            ],
            &current_version,
        );
        let unassigned_version = current_version.clone();
        let unassigned_nodes_proposal = vec![UpdateUnassignedNodesProposal {
            info: ProposalInfoInternal {
                executed: true,
                executed_timestamp_seconds: 0,
                id: 5,
                proposal_timestamp_seconds: 0,
            },
            payload: UpdateUnassignedNodesConfigPayload {
                ssh_readonly_access: None,
                replica_version: Some(current_version.clone()),
            },
        }];
        let mut subnets = craft_subnets();
        replace_versions(
            &mut subnets,
            &[
                ("io67a", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("shefu", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("uzr34", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
            ],
        );
        let now = NaiveDate::parse_from_str("2024-02-28", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposal,
            index,
            None,
            &unassigned_version,
            &subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();
        println!("{:#?}", actions);
        assert_eq!(actions.len(), 1);
        for action in actions {
            match action {
                SubnetAction::PlaceProposal {
                    is_unassigned,
                    subnet_principal,
                    version,
                } => {
                    assert!(subnet_principal.starts_with("pjljw"));
                    assert_eq!(is_unassigned, false);
                    assert_eq!(version, current_version)
                }
                // Just fail
                _ => assert!(false),
            }
        }
    }

    /// Use case 9: Next monday came, proposal for last subnet executed and bake time passed. Rollout finished
    ///
    /// `last_bake_status` - contains the status for all subnets before unassigned nodes
    /// `subnet_update_proposals` - contains proposals from previous two stages
    /// `unassigned_nodes_proposals` - contains executed proposal for unassigned nodes
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-26`
    #[test]
    fn test_use_case_9() {
        let index = craft_index_state();
        let current_version = "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string();
        let last_bake_status = [
            (
                "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
                humantime::parse_duration("9h"),
            ),
            (
                "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
                humantime::parse_duration("5h"),
            ),
            (
                "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
                humantime::parse_duration("5h"),
            ),
            (
                "pjljw-kztyl-46ud4-ofrj6-nzkhm-3n4nt-wi3jt-ypmav-ijqkt-gjf66-uae",
                humantime::parse_duration("5h"),
            ),
        ]
        .iter()
        .map(|(id, duration)| {
            (
                id.to_string(),
                duration.clone().expect("Should parse duration").as_secs_f64(),
            )
        })
        .collect();
        let subnet_update_proposals = craft_executed_proposals(
            &[
                "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
                "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
                "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
                "pjljw-kztyl-46ud4-ofrj6-nzkhm-3n4nt-wi3jt-ypmav-ijqkt-gjf66-uae",
            ],
            &current_version,
        );
        let unassigned_version = current_version.clone();
        let unassigned_nodes_proposal = vec![UpdateUnassignedNodesProposal {
            info: ProposalInfoInternal {
                executed: true,
                executed_timestamp_seconds: 0,
                id: 5,
                proposal_timestamp_seconds: 0,
            },
            payload: UpdateUnassignedNodesConfigPayload {
                ssh_readonly_access: None,
                replica_version: Some(current_version.clone()),
            },
        }];
        let mut subnets = craft_subnets();
        replace_versions(
            &mut subnets,
            &[
                ("io67a", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("shefu", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("uzr34", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
                ("pjljw", "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f"),
            ],
        );
        let now = NaiveDate::parse_from_str("2024-02-28", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposal,
            index,
            None,
            &unassigned_version,
            &subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();
        assert_eq!(actions.len(), 0);
    }
}

// E2E tests for decision making process for happy path with feature builds
#[cfg(test)]
mod check_stages_tests_feature_builds {
    use std::str::FromStr;

    use candid::Principal;
    use check_stages_tests_feature_builds::check_stages_tests_no_feature_builds::{craft_subnets, replace_versions};
    use ic_base_types::PrincipalId;
    use ic_management_backend::proposal::ProposalInfoInternal;
    use registry_canister::mutations::do_update_subnet_replica::UpdateSubnetReplicaVersionPayload;

    use crate::calculation::{Index, Release, Rollout, Version};

    use super::*;

    /// Part two => Feature builds
    /// `last_bake_status` - can be defined depending on the use case
    /// `subnet_update_proposals` - can be defined depending on the use case
    /// `unassigned_nodes_update_proposals` - can be defined depending on the use case
    /// `index` - has to be defined
    /// `logger` - can be defined, but won't be because these are only tests
    /// `subnets` - has to be defined
    /// `now` - has to be defined
    ///
    /// For all use cases we will use the following setup
    /// rollout:
    ///     pause: false // Tested in `should_proceed.rs` module
    ///     skip_days: [] // Tested in `should_proceed.rs` module
    ///     stages:
    ///         - subnets: [io67a]
    ///           bake_time: 8h
    ///         - subnets: [shefu, uzr34]
    ///           bake_time: 4h
    ///         - update_unassigned_nodes: true
    ///         - subnets: [pjljw]
    ///           wait_for_next_week: true
    ///           bake_time: 4h
    /// releases:
    ///     - rc_name: rc--2024-02-21_23-01
    ///       versions:
    ///         - version: 2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f
    ///           name: rc--2024-02-21_23-01
    ///           release_notes_ready: <not-important>
    ///           subnets: []
    ///         - version: 76521ef765e86187c43f7d6a02e63332a6556c8c
    ///           name: rc--2024-02-21_23-01-feat
    ///           release_notes_ready: <not-important>
    ///           subnets:
    ///             - shefu
    ///             - io67a
    ///     - rc_name: rc--2024-02-14_23-01
    ///       versions:
    ///         - version: 85bd56a70e55b2cea75cae6405ae11243e5fdad8
    ///           name: rc--2024-02-14_23-01
    ///           release_notes_ready: <not-important>
    ///           subnets: [] // empty because its a regular build
    fn craft_index_state() -> Index {
        Index {
            rollout: Rollout {
                pause: false,
                skip_days: vec![],
                stages: vec![
                    Stage {
                        subnets: vec!["io67a".to_string()],
                        bake_time: humantime::parse_duration("8h").expect("Should be able to parse."),
                        ..Default::default()
                    },
                    Stage {
                        subnets: vec!["shefu".to_string(), "uzr34".to_string()],
                        bake_time: humantime::parse_duration("4h").expect("Should be able to parse."),
                        ..Default::default()
                    },
                    Stage {
                        update_unassigned_nodes: true,
                        ..Default::default()
                    },
                    Stage {
                        subnets: vec!["pjljw".to_string()],
                        bake_time: humantime::parse_duration("4h").expect("Should be able to parse."),
                        wait_for_next_week: true,
                        ..Default::default()
                    },
                ],
            },
            releases: vec![
                Release {
                    rc_name: "rc--2024-02-21_23-01".to_string(),
                    versions: vec![
                        Version {
                            name: "rc--2024-02-21_23-01".to_string(),
                            version: "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string(),
                            ..Default::default()
                        },
                        Version {
                            name: "rc--2024-02-21_23-01-feat".to_string(),
                            version: "76521ef765e86187c43f7d6a02e63332a6556c8c".to_string(),
                            subnets: ["io67a", "shefu"].iter().map(|f| f.to_string()).collect(),
                            ..Default::default()
                        },
                    ],
                },
                Release {
                    rc_name: "rc--2024-02-14_23-01".to_string(),
                    versions: vec![Version {
                        name: "rc--2024-02-14_23-01".to_string(),
                        version: "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string(),
                        ..Default::default()
                    }],
                },
            ],
        }
    }

    /// Use-Case 1: Beginning of a new rollout
    ///
    /// `last_bake_status` - empty, because no subnets have the version
    /// `subnet_update_proposals` - can be empty but doesn't have to be. For e.g. if its Monday it is possible to have an open proposal for NNS
    ///                             But it is for a different version (one from last week)
    /// `unassigned_nodes_proposals` - empty
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-21`
    #[test]
    fn test_use_case_1() {
        let index = craft_index_state();
        let last_bake_status = BTreeMap::new();
        let subnet_update_proposals = Vec::new();
        let unassigned_version = "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string();
        let unassigned_nodes_proposals = vec![];
        let subnets = &craft_subnets();
        let feature = index
            .releases
            .get(0)
            .expect("Should be at least one")
            .versions
            .get(1)
            .expect("Should be set to be the second version being rolled out");
        // TODO: replace in index
        let mut current_release_feature_spec = BTreeMap::new();
        current_release_feature_spec.insert(feature.version.clone(), feature.subnets.clone());
        let now = NaiveDate::parse_from_str("2024-02-21", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposals,
            index.clone(),
            None,
            &unassigned_version,
            subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();

        assert_eq!(actions.len(), 1);
        for action in actions {
            match action {
                SubnetAction::PlaceProposal {
                    is_unassigned,
                    subnet_principal,
                    version,
                } => {
                    assert_eq!(is_unassigned, false);
                    assert_eq!(version, feature.version);
                    assert!(subnet_principal.starts_with("io67a"))
                }
                // Fail the test
                _ => assert!(false),
            }
        }
    }

    /// Use case 2: First batch is submitted the proposal was executed and the subnet is baked, placing proposal for next stage
    ///
    /// `last_bake_status` - contains the status for the first subnet
    /// `subnet_update_proposals` - contains proposals from the first stage
    /// `unassigned_nodes_proposals` - empty
    /// `subnets` - can be seen in `craft_index_state`
    /// `now` - same `2024-02-21`
    #[test]
    fn test_use_case_2() {
        let index = craft_index_state();
        let current_version = "2e921c9adfc71f3edc96a9eb5d85fc742e7d8a9f".to_string();
        let last_bake_status = [(
            "io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe",
            humantime::parse_duration("9h"),
        )]
        .iter()
        .map(|(id, duration)| {
            (
                id.to_string(),
                duration.clone().expect("Should parse duration").as_secs_f64(),
            )
        })
        .collect();
        let subnet_principal = Principal::from_str("io67a-2jmkw-zup3h-snbwi-g6a5n-rm5dn-b6png-lvdpl-nqnto-yih6l-gqe")
            .expect("Should be possible to create principal");
        let subnet_update_proposals = vec![SubnetUpdateProposal {
            info: ProposalInfoInternal {
                executed: true,
                executed_timestamp_seconds: 0,
                proposal_timestamp_seconds: 0,
                id: 1,
            },
            payload: UpdateSubnetReplicaVersionPayload {
                subnet_id: PrincipalId(subnet_principal.clone()),
                replica_version_id: "76521ef765e86187c43f7d6a02e63332a6556c8c".to_string(),
            },
        }];
        let unassigned_version = "85bd56a70e55b2cea75cae6405ae11243e5fdad8".to_string();
        let unassigned_nodes_proposals = vec![];
        let mut subnets = craft_subnets();
        replace_versions(&mut subnets, &[("io67a", "76521ef765e86187c43f7d6a02e63332a6556c8c")]);
        let feature = index
            .releases
            .get(0)
            .expect("Should be at least one")
            .versions
            .get(1)
            .expect("Should be set to be the second version being rolled out");
        // TODO: replace in index
        let mut current_release_feature_spec = BTreeMap::new();
        current_release_feature_spec.insert(feature.version.clone(), feature.subnets.clone());
        let now = NaiveDate::parse_from_str("2024-02-21", "%Y-%m-%d").expect("Should parse date");

        let maybe_actions = check_stages(
            &last_bake_status,
            &subnet_update_proposals,
            &unassigned_nodes_proposals,
            index.clone(),
            None,
            &unassigned_version,
            &subnets,
            now,
        );

        assert!(maybe_actions.is_ok());
        let actions = maybe_actions.unwrap();
        println!("{:#?}", actions);
        assert_eq!(actions.len(), 2);
        let subnets = vec![
            "shefu-t3kr5-t5q3w-mqmdq-jabyv-vyvtf-cyyey-3kmo4-toyln-emubw-4qe",
            "uzr34-akd3s-xrdag-3ql62-ocgoh-ld2ao-tamcv-54e7j-krwgb-2gm4z-oqe",
        ];
        for action in actions {
            match action {
                SubnetAction::PlaceProposal {
                    is_unassigned,
                    subnet_principal,
                    version,
                } => {
                    assert_eq!(is_unassigned, false);
                    if subnet_principal.starts_with("shefu") {
                        assert_eq!(version, feature.version);
                    } else {
                        assert_eq!(version, current_version);
                    }
                    assert!(subnets.contains(&subnet_principal.as_str()))
                }
                // Just fail
                _ => assert!(false),
            }
        }
    }
}
