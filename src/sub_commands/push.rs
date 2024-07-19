use anyhow::{bail, Context, Result};

#[cfg(not(test))]
use crate::client::Client;
#[cfg(test)]
use crate::client::MockConnect;
use crate::{
    client::{fetching_with_report, get_repo_ref_from_cache, Connect},
    git::{str_to_sha1, Repo, RepoActions},
    login,
    repo_ref::get_repo_coordinates,
    sub_commands::{
        self,
        list::{
            get_all_proposal_patch_events_from_cache, get_commit_id_from_patch,
            get_most_recent_patch_with_ancestors, get_proposals_and_revisions_from_cache,
            tag_value,
        },
        send::{event_to_cover_letter, generate_patch_event, send_events},
    },
    Cli,
};

#[derive(Debug, clap::Args)]
pub struct SubCommandArgs {
    #[arg(long, action)]
    /// send proposal revision from checked out proposal branch
    force: bool,
    #[arg(long, action)]
    /// dont prompt for cover letter when force pushing
    no_cover_letter: bool,
}

#[allow(clippy::too_many_lines)]
pub async fn launch(cli_args: &Cli, args: &SubCommandArgs) -> Result<()> {
    let git_repo = Repo::discover().context("cannot find a git repository")?;
    let git_repo_path = git_repo.get_path()?;

    let (main_or_master_branch_name, _) = git_repo
        .get_main_or_master_branch()
        .context("no main or master branch")?;

    let root_commit = git_repo
        .get_root_commit()
        .context("failed to get root commit of the repository")?;

    let branch_name = git_repo
        .get_checked_out_branch_name()
        .context("cannot get checked out branch name")?;

    if branch_name == main_or_master_branch_name {
        bail!("checkout a branch associated with a proposal first")
    }
    #[cfg(not(test))]
    let mut client = Client::default();
    #[cfg(test)]
    let mut client = <MockConnect as std::default::Default>::default();

    let repo_coordinates = get_repo_coordinates(&git_repo, &client).await?;

    fetching_with_report(git_repo_path, &client, &repo_coordinates).await?;

    let repo_ref = get_repo_ref_from_cache(git_repo_path, &repo_coordinates).await?;

    let proposal_root_event =
        get_proposals_and_revisions_from_cache(git_repo_path, repo_ref.coordinates())
            .await?
            .iter()
            .find(|e| event_to_cover_letter(e).is_ok_and(|cl| cl.branch_name.eq(&branch_name)))
            .context("cannot find proposal that matches the current branch name")?
            .clone();

    let commit_events = get_all_proposal_patch_events_from_cache(
        git_repo_path,
        &repo_ref,
        &proposal_root_event.id(),
    )
    .await?;

    let most_recent_proposal_patch_chain = get_most_recent_patch_with_ancestors(commit_events)
        .context("cannot get most recent patch for proposal")?;

    let branch_tip = git_repo.get_tip_of_branch(&branch_name)?;

    let most_recent_patch_commit_id = str_to_sha1(
        &get_commit_id_from_patch(
            most_recent_proposal_patch_chain
                .first()
                .context("no patches found")?,
        )
        .context("latest patch event doesnt have a commit tag")?,
    )
    .context("latest patch event commit tag isn't a valid SHA1 hash")?;

    let proposal_base_commit_id = str_to_sha1(
        &tag_value(
            most_recent_proposal_patch_chain
                .last()
                .context("no patches found")?,
            "parent-commit",
        )
        .context("patch is incorrectly formatted")?,
    )
    .context("latest patch event parent-commit tag isn't a valid SHA1 hash")?;

    if most_recent_patch_commit_id.eq(&branch_tip) {
        bail!("proposal already up-to-date with local branch");
    }

    if args.force {
        println!("preparing to force push proposal revision...");
        sub_commands::send::launch(
            cli_args,
            &sub_commands::send::SubCommandArgs {
                since_or_range: String::new(),
                in_reply_to: vec![proposal_root_event.id.to_string()],
                title: None,
                description: None,
                no_cover_letter: args.no_cover_letter,
            },
            true,
        )
        .await?;
        println!("force pushed proposal revision");
        return Ok(());
    }

    if most_recent_proposal_patch_chain.iter().any(|e| {
        let c = tag_value(e, "parent-commit").unwrap_or_default();
        c.eq(&branch_tip.to_string())
    }) {
        bail!("proposal is ahead of local branch");
    }

    let Ok((ahead, behind)) = git_repo
        .get_commits_ahead_behind(&most_recent_patch_commit_id, &branch_tip)
        .context("the latest patch in proposal doesnt share an ancestor with your branch.")
    else {
        if git_repo.ancestor_of(&proposal_base_commit_id, &branch_tip)? {
            bail!("local unpublished proposal ammendments. consider force pushing.");
        }
        bail!("local unpublished proposal has been rebased. consider force pushing");
    };

    if !behind.is_empty() {
        bail!(
            "your local proposal branch is {} behind patches on nostr. consider rebasing or force pushing",
            behind.len()
        )
    }

    println!(
        "{} commits ahead. preparing to create creating patch events.",
        ahead.len()
    );

    let (signer, user_ref) = login::launch(
        &git_repo,
        &cli_args.bunker_uri,
        &cli_args.bunker_app_key,
        &cli_args.nsec,
        &cli_args.password,
        Some(&client),
        false,
    )
    .await?;

    let mut patch_events: Vec<nostr::Event> = vec![];
    for commit in &ahead {
        patch_events.push(
            generate_patch_event(
                &git_repo,
                &root_commit,
                commit,
                Some(proposal_root_event.id),
                &signer,
                &repo_ref,
                patch_events.last().map(nostr::Event::id),
                None,
                None,
                &None,
                &[],
            )
            .await
            .context("cannot make patch event from commit")?,
        );
    }
    println!("pushing {} commits", ahead.len());

    client.set_signer(signer).await;

    send_events(
        &client,
        patch_events,
        user_ref.relays.write(),
        repo_ref.relays.clone(),
        !cli_args.disable_cli_spinners,
    )
    .await?;

    println!("pushed {} commits", ahead.len());

    Ok(())
}
