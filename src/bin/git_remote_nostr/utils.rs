use core::str;
use std::{
    collections::HashMap,
    io::{self, Stdin},
};

use anyhow::{bail, Context, Result};
use git2::Repository;
use ngit::{
    client::{
        get_all_proposal_patch_events_from_cache, get_events_from_cache,
        get_proposals_and_revisions_from_cache,
    },
    git::{
        nostr_url::{CloneUrl, NostrUrlDecoded, ServerProtocol},
        Repo, RepoActions,
    },
    git_events::{
        event_is_revision_root, event_to_cover_letter, get_most_recent_patch_with_ancestors,
        status_kinds,
    },
    repo_ref::RepoRef,
};
use nostr_sdk::{Event, EventId, Kind, PublicKey, Url};

pub fn get_short_git_server_name(git_repo: &Repo, url: &str) -> std::string::String {
    if let Ok(name) = get_remote_name_by_url(&git_repo.git_repo, url) {
        return name;
    }
    if let Ok(url) = Url::parse(url) {
        if let Some(domain) = url.domain() {
            return domain.to_string();
        }
    }
    url.to_string()
}

pub fn get_remote_name_by_url(git_repo: &Repository, url: &str) -> Result<String> {
    let remotes = git_repo.remotes()?;
    Ok(remotes
        .iter()
        .find(|r| {
            if let Some(name) = r {
                if let Some(remote_url) = git_repo.find_remote(name).unwrap().url() {
                    url == remote_url
                } else {
                    false
                }
            } else {
                false
            }
        })
        .context("could not find remote with matching url")?
        .context("remote with matching url must be named")?
        .to_string())
}

pub fn get_oids_from_fetch_batch(
    stdin: &Stdin,
    initial_oid: &str,
    initial_refstr: &str,
) -> Result<HashMap<String, String>> {
    let mut line = String::new();
    let mut batch = HashMap::new();
    batch.insert(initial_refstr.to_string(), initial_oid.to_string());
    loop {
        let tokens = read_line(stdin, &mut line)?;
        match tokens.as_slice() {
            ["fetch", oid, refstr] => {
                batch.insert((*refstr).to_string(), (*oid).to_string());
            }
            [] => break,
            _ => bail!(
                "after a `fetch` command we are only expecting another fetch or an empty line"
            ),
        }
    }
    Ok(batch)
}

/// Read one line from stdin, and split it into tokens.
pub fn read_line<'a>(stdin: &io::Stdin, line: &'a mut String) -> io::Result<Vec<&'a str>> {
    line.clear();

    let read = stdin.read_line(line)?;
    if read == 0 {
        return Ok(vec![]);
    }
    let line = line.trim();
    let tokens = line.split(' ').filter(|t| !t.is_empty()).collect();

    Ok(tokens)
}

pub async fn get_open_proposals(
    git_repo: &Repo,
    repo_ref: &RepoRef,
) -> Result<HashMap<EventId, (Event, Vec<Event>)>> {
    let git_repo_path = git_repo.get_path()?;
    let proposals: Vec<nostr::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates())
            .await?
            .iter()
            .filter(|e| !event_is_revision_root(e))
            .cloned()
            .collect();

    let statuses: Vec<nostr::Event> = {
        let mut statuses = get_events_from_cache(
            git_repo_path,
            vec![
                nostr::Filter::default()
                    .kinds(status_kinds().clone())
                    .events(proposals.iter().map(nostr::Event::id)),
            ],
        )
        .await?;
        statuses.sort_by_key(|e| e.created_at);
        statuses.reverse();
        statuses
    };
    let mut open_proposals = HashMap::new();

    for proposal in proposals {
        let status = if let Some(e) = statuses
            .iter()
            .filter(|e| {
                status_kinds().contains(&e.kind())
                    && e.tags()
                        .iter()
                        .any(|t| t.as_vec()[1].eq(&proposal.id.to_string()))
            })
            .collect::<Vec<&nostr::Event>>()
            .first()
        {
            e.kind()
        } else {
            Kind::GitStatusOpen
        };
        if status.eq(&Kind::GitStatusOpen) {
            if let Ok(commits_events) =
                get_all_proposal_patch_events_from_cache(git_repo_path, repo_ref, &proposal.id)
                    .await
            {
                if let Ok(most_recent_proposal_patch_chain) =
                    get_most_recent_patch_with_ancestors(commits_events.clone())
                {
                    open_proposals
                        .insert(proposal.id(), (proposal, most_recent_proposal_patch_chain));
                }
            }
        }
    }
    Ok(open_proposals)
}

pub async fn get_all_proposals(
    git_repo: &Repo,
    repo_ref: &RepoRef,
) -> Result<HashMap<EventId, (Event, Vec<Event>)>> {
    let git_repo_path = git_repo.get_path()?;
    let proposals: Vec<nostr::Event> =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates())
            .await?
            .iter()
            .filter(|e| !event_is_revision_root(e))
            .cloned()
            .collect();

    let mut all_proposals = HashMap::new();

    for proposal in proposals {
        if let Ok(commits_events) =
            get_all_proposal_patch_events_from_cache(git_repo_path, repo_ref, &proposal.id).await
        {
            if let Ok(most_recent_proposal_patch_chain) =
                get_most_recent_patch_with_ancestors(commits_events.clone())
            {
                all_proposals.insert(proposal.id(), (proposal, most_recent_proposal_patch_chain));
            }
        }
    }
    Ok(all_proposals)
}

pub fn find_proposal_and_patches_by_branch_name<'a>(
    refstr: &'a str,
    open_proposals: &'a HashMap<EventId, (Event, Vec<Event>)>,
    current_user: &Option<PublicKey>,
) -> Option<(&'a EventId, &'a (Event, Vec<Event>))> {
    open_proposals.iter().find(|(_, (proposal, _))| {
        if let Ok(cl) = event_to_cover_letter(proposal) {
            if let Ok(mut branch_name) = cl.get_branch_name() {
                branch_name = if let Some(public_key) = current_user {
                    if proposal.author().eq(public_key) {
                        cl.branch_name.to_string()
                    } else {
                        branch_name
                    }
                } else {
                    branch_name
                };
                branch_name.eq(&refstr.replace("refs/heads/", ""))
            } else {
                false
            }
        } else {
            false
        }
    })
}

pub fn join_with_and<T: ToString>(items: &[T]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].to_string(),
        _ => {
            let last_item = items.last().unwrap().to_string();
            let rest = &items[..items.len() - 1];
            format!(
                "{} and {}",
                rest.iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                last_item
            )
        }
    }
}

/// get an ordered vector of server protocols to attempt
pub fn get_read_protocols_to_try(
    server_url: &CloneUrl,
    decoded_nostr_url: &NostrUrlDecoded,
) -> Vec<ServerProtocol> {
    if server_url.protocol() == ServerProtocol::Filesystem {
        vec![(ServerProtocol::Filesystem)]
    } else if let Some(protocol) = &decoded_nostr_url.protocol {
        vec![protocol.clone()]
    } else if server_url.protocol() == ServerProtocol::Http {
        vec![
            ServerProtocol::UnauthHttp,
            ServerProtocol::Ssh,
            // note: list and fetch stop here if ssh was authenticated
            ServerProtocol::Http,
        ]
    } else if server_url.protocol() == ServerProtocol::Ftp {
        vec![ServerProtocol::Ftp, ServerProtocol::Ssh]
    } else {
        vec![
            ServerProtocol::UnauthHttps,
            ServerProtocol::Ssh,
            // note: list and fetch stop here if ssh was authenticated
            ServerProtocol::Https,
        ]
    }
}

/// get an ordered vector of server protocols to attempt
pub fn get_write_protocols_to_try(
    server_url: &CloneUrl,
    decoded_nostr_url: &NostrUrlDecoded,
) -> Vec<ServerProtocol> {
    if server_url.protocol() == ServerProtocol::Filesystem {
        vec![(ServerProtocol::Filesystem)]
    } else if let Some(protocol) = &decoded_nostr_url.protocol {
        vec![protocol.clone()]
    } else if server_url.protocol() == ServerProtocol::Http {
        vec![
            ServerProtocol::Ssh,
            // note: list and fetch stop here if ssh was authenticated
            ServerProtocol::Http,
        ]
    } else if server_url.protocol() == ServerProtocol::Ftp {
        vec![ServerProtocol::Ssh, ServerProtocol::Ftp]
    } else {
        vec![
            ServerProtocol::Ssh,
            // note: list and fetch stop here if ssh was authenticated
            ServerProtocol::Https,
        ]
    }
}

/// to understand whether to try over another protocol
pub fn fetch_or_list_error_is_not_authentication_failure(error: &anyhow::Error) -> bool {
    !error_might_be_authentication_related(error)
}

/// to understand whether to try over another protocol
pub fn push_error_is_not_authentication_failure(error: &anyhow::Error) -> bool {
    !error_might_be_authentication_related(error)
}

pub fn error_might_be_authentication_related(error: &anyhow::Error) -> bool {
    let error_str = error.to_string();
    for s in [
        "no ssh keys found",
        "invalid or unknown remote ssh hostkey",
        "all authentication attempts failed",
        "Permission to",
        "Repository not found",
    ] {
        if error_str.contains(s) {
            return true;
        }
    }
    false
}

pub enum TransferDirection {
    Fetch,
    Push,
}

pub enum ProgressStatus {
    InProgress,
    Complete,
}

#[allow(clippy::cast_precision_loss)]
#[allow(clippy::float_cmp)]
#[allow(clippy::needless_pass_by_value)]
pub fn report_on_transfer_progress(
    progress_stats: &git2::Progress<'_>,
    term: &console::Term,
    direction: TransferDirection,
    status: ProgressStatus,
) {
    let total = progress_stats.total_objects() as f64;
    if total == 0.0 {
        return;
    }
    let received = progress_stats.received_objects() as f64;
    let percentage = (received / total) * 100.0;

    // Get the total received bytes
    let received_bytes = progress_stats.received_bytes() as f64;

    // Determine whether to use KiB or MiB
    let (size, unit) = if received_bytes >= (1024.0 * 1024.0) {
        // Convert to MiB
        (received_bytes / (1024.0 * 1024.0), "MiB")
    } else {
        // Convert to KiB
        (received_bytes / 1024.0, "KiB")
    };

    // Format the output for receiving objects
    if received < total || matches!(status, ProgressStatus::Complete) {
        let _ = term.write_line(
            format!(
                "{} objects: {percentage:.0}% ({received}/{total}) {size:.2} {unit}, done.\r",
                if matches!(direction, TransferDirection::Fetch) {
                    "Receiving"
                } else {
                    "Writing"
                },
            )
            .as_str(),
        );
    }
    if received == total || matches!(status, ProgressStatus::Complete) {
        let indexed_deltas = progress_stats.indexed_deltas() as f64;
        let total_deltas = progress_stats.total_deltas() as f64;
        let percentage = (indexed_deltas / total_deltas) * 100.0;
        let _ = term.write_line(
            format!("Resolving deltas: {percentage:.0}% ({indexed_deltas}/{total_deltas}) done.\r")
                .as_str(),
        );
    }
}

pub fn report_on_sideband_progress(data: &[u8], term: &console::Term) {
    if let Ok(data) = str::from_utf8(data) {
        let data = data
            .split(['\n', '\r'])
            .find(|line| !line.is_empty())
            .unwrap_or("");
        if !data.is_empty() {
            let s = format!("remote: {data}");
            let _ = term.clear_last_lines(1);
            let _ = term.write_line(s.as_str());
            if !s.contains('%') || s.contains("100%") {
                // print it twice so the next sideband_progress doesn't delete it
                let _ = term.write_line(s.as_str());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    mod join_with_and {
        use super::*;
        #[test]
        fn test_empty() {
            let items: Vec<&str> = vec![];
            assert_eq!(join_with_and(&items), "");
        }

        #[test]
        fn test_single_item() {
            let items = vec!["apple"];
            assert_eq!(join_with_and(&items), "apple");
        }

        #[test]
        fn test_two_items() {
            let items = vec!["apple", "banana"];
            assert_eq!(join_with_and(&items), "apple and banana");
        }

        #[test]
        fn test_three_items() {
            let items = vec!["apple", "banana", "cherry"];
            assert_eq!(join_with_and(&items), "apple, banana and cherry");
        }

        #[test]
        fn test_four_items() {
            let items = vec!["apple", "banana", "cherry", "date"];
            assert_eq!(join_with_and(&items), "apple, banana, cherry and date");
        }

        #[test]
        fn test_multiple_items() {
            let items = vec!["one", "two", "three", "four", "five"];
            assert_eq!(join_with_and(&items), "one, two, three, four and five");
        }
    }
}
