// have you considered

// TO USE ASYNC

// in traits (required for mocking unit tests)
// https://rust-lang.github.io/async-book/07_workarounds/05_async_in_traits.html
// https://github.com/dtolnay/async-trait
// see https://blog.rust-lang.org/inside-rust/2022/11/17/async-fn-in-trait-nightly.html
// I think we can use the async-trait crate and switch to the native feature
// which is currently in nightly. alternatively we can use nightly as it looks
// certain that the implementation is going to make it to stable but we don't
// want to inadvertlty use other features of nightly that might be removed.
use std::{
    collections::{HashMap, HashSet},
    fmt::{Display, Write},
    fs::create_dir_all,
    path::Path,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use console::Style;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};
#[cfg(test)]
use mockall::*;
use nostr::{nips::nip01::Coordinate, Event};
use nostr_database::{NostrDatabase, Order};
use nostr_sdk::{
    prelude::RelayLimits, EventBuilder, EventId, Kind, NostrSigner, Options, PublicKey,
    SingleLetterTag, Timestamp, Url,
};
use nostr_sqlite::SQLiteDatabase;

use crate::{
    config::get_dirs,
    repo_ref::{RepoRef, REPO_REF_KIND},
    sub_commands::{
        list::status_kinds,
        send::{event_is_patch_set_root, PATCH_KIND},
    },
};

#[allow(clippy::struct_field_names)]
pub struct Client {
    client: nostr_sdk::Client,
    fallback_relays: Vec<String>,
    more_fallback_relays: Vec<String>,
    blaster_relays: Vec<String>,
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait Connect {
    fn default() -> Self;
    fn new(opts: Params) -> Self;
    async fn set_signer(&mut self, signer: NostrSigner);
    async fn connect(&self, relay_url: &Url) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;
    fn get_fallback_relays(&self) -> &Vec<String>;
    fn get_more_fallback_relays(&self) -> &Vec<String>;
    fn get_blaster_relays(&self) -> &Vec<String>;
    async fn send_event_to(&self, url: &str, event: nostr::event::Event) -> Result<nostr::EventId>;
    async fn get_events(
        &self,
        relays: Vec<String>,
        filters: Vec<nostr::Filter>,
    ) -> Result<Vec<nostr::Event>>;
    async fn get_events_per_relay(
        &self,
        relays: Vec<Url>,
        filters: Vec<nostr::Filter>,
        progress_reporter: MultiProgress,
    ) -> Result<(Vec<Result<Vec<nostr::Event>>>, MultiProgress)>;
    async fn fetch_all(
        &self,
        git_repo_path: &Path,
        repo_coordinates: &HashSet<Coordinate>,
    ) -> Result<FetchReport>;
    async fn fetch_all_from_relay(
        &self,
        git_repo_path: &Path,
        relay_url: Url,
        request: FetchRequest,
        // progress_reporter: &MultiProgress,
        pb: &Option<ProgressBar>,
    ) -> Result<FetchReport>;
}

#[async_trait]
impl Connect for Client {
    fn default() -> Self {
        let fallback_relays: Vec<String> = if std::env::var("NGITTEST").is_ok() {
            vec![
                "ws://localhost:8051".to_string(),
                "ws://localhost:8052".to_string(),
            ]
        } else {
            vec![
                "wss://relay.damus.io".to_string(), /* free, good reliability, have been known
                                                     * to delete all messages */
                "wss://nos.lol".to_string(),
                "wss://relay.nostr.band".to_string(),
            ]
        };

        let more_fallback_relays: Vec<String> = if std::env::var("NGITTEST").is_ok() {
            vec![
                "ws://localhost:8055".to_string(),
                "ws://localhost:8056".to_string(),
            ]
        } else {
            vec![
                "wss://purplerelay.com".to_string(), // free but reliability not tested
                "wss://purplepages.es".to_string(),  // for profile events but unreliable
                "wss://relayable.org".to_string(),   // free but not always reliable
            ]
        };

        let blaster_relays: Vec<String> = if std::env::var("NGITTEST").is_ok() {
            vec!["ws://localhost:8057".to_string()]
        } else {
            vec!["wss://nostr.mutinywallet.com".to_string()]
        };
        Client {
            client: nostr_sdk::ClientBuilder::new()
                .opts(Options::new().relay_limits(RelayLimits::disable()))
                .build(),
            fallback_relays,
            more_fallback_relays,
            blaster_relays,
        }
    }
    fn new(opts: Params) -> Self {
        Client {
            client: nostr_sdk::ClientBuilder::new()
                .opts(Options::new().relay_limits(RelayLimits::disable()))
                .signer(&opts.keys.unwrap_or(nostr::Keys::generate()))
                // .database(
                //     SQLiteDatabase::open(get_dirs()?.config_dir().join("cache.sqlite")).await?,
                // )
                .build(),
            fallback_relays: opts.fallback_relays,
            more_fallback_relays: opts.more_fallback_relays,
            blaster_relays: opts.blaster_relays,
        }
    }

    async fn set_signer(&mut self, signer: NostrSigner) {
        self.client.set_signer(Some(signer)).await;
    }

    async fn connect(&self, relay_url: &Url) -> Result<()> {
        self.client
            .add_relay(relay_url)
            .await
            .context("cannot add relay")?;

        let relay = self.client.relay(relay_url).await?;

        if !relay.is_connected().await {
            #[allow(clippy::large_futures)]
            relay
                .connect(Some(std::time::Duration::from_secs(CONNECTION_TIMEOUT)))
                .await;
        }

        if !relay.is_connected().await {
            bail!("connection timeout");
        }
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.client.disconnect().await?;
        Ok(())
    }

    fn get_fallback_relays(&self) -> &Vec<String> {
        &self.fallback_relays
    }

    fn get_more_fallback_relays(&self) -> &Vec<String> {
        &self.more_fallback_relays
    }

    fn get_blaster_relays(&self) -> &Vec<String> {
        &self.blaster_relays
    }

    async fn send_event_to(&self, url: &str, event: Event) -> Result<nostr::EventId> {
        self.client.add_relay(url).await?;
        #[allow(clippy::large_futures)]
        self.client.connect_relay(url).await?;
        Ok(self.client.send_event_to(vec![url], event).await?)
    }

    async fn get_events(
        &self,
        relays: Vec<String>,
        filters: Vec<nostr::Filter>,
    ) -> Result<Vec<nostr::Event>> {
        let (relay_results, _) = self
            .get_events_per_relay(
                relays.iter().map(|r| Url::parse(r).unwrap()).collect(),
                filters,
                MultiProgress::new(),
            )
            .await?;
        Ok(get_dedup_events(relay_results))
    }

    async fn get_events_per_relay(
        &self,
        relays: Vec<Url>,
        filters: Vec<nostr::Filter>,
        progress_reporter: MultiProgress,
    ) -> Result<(Vec<Result<Vec<nostr::Event>>>, MultiProgress)> {
        // add relays
        for relay in &relays {
            self.client
                .add_relay(relay.as_str())
                .await
                .context("cannot add relay")?;
        }

        let relays_map = self.client.relays().await;

        let futures: Vec<_> = relays
            .clone()
            .iter()
            // don't look for events on blaster
            .filter(|r| !r.as_str().contains("nostr.mutinywallet.com"))
            .map(|r| (relays_map.get(r).unwrap(), filters.clone()))
            .map(|(relay, filters)| async {
                let pb = if std::env::var("NGITTEST").is_err() {
                    let pb = progress_reporter.add(
                        ProgressBar::new(1)
                            .with_prefix(format!("{: <11}{}", "connecting", relay.url()))
                            .with_style(pb_style()?),
                    );
                    pb.enable_steady_tick(Duration::from_millis(300));
                    Some(pb)
                } else {
                    None
                };
                #[allow(clippy::large_futures)]
                match get_events_of(relay, filters, &pb).await {
                    Err(error) => {
                        if let Some(pb) = pb {
                            pb.set_style(pb_after_style(false));
                            pb.set_prefix(format!("{: <11}{}", "error", relay.url()));
                            pb.finish_with_message(
                                console::style(
                                    error.to_string().replace("relay pool error:", "error:"),
                                )
                                .for_stderr()
                                .red()
                                .to_string(),
                            );
                        }
                        Err(error)
                    }
                    Ok(res) => {
                        if let Some(pb) = pb {
                            pb.set_style(pb_after_style(true));
                            pb.set_prefix(format!(
                                "{: <11}{}",
                                format!("{} events", res.len()),
                                relay.url()
                            ));
                            pb.finish_with_message("");
                        }
                        Ok(res)
                    }
                }
            })
            .collect();

        let relay_results: Vec<Result<Vec<nostr::Event>>> =
            stream::iter(futures).buffer_unordered(15).collect().await;

        Ok((relay_results, progress_reporter))
    }

    #[allow(clippy::too_many_lines)]
    async fn fetch_all(
        &self,
        git_repo_path: &Path,
        repo_coordinates: &HashSet<Coordinate>,
    ) -> Result<FetchReport> {
        println!("fetching updates...");
        let mut fallback_relays = HashSet::new();
        for r in &self.fallback_relays {
            if let Ok(url) = Url::parse(r) {
                fallback_relays.insert(url);
            }
        }
        let (relays, request) =
            create_relays_request(git_repo_path, repo_coordinates, fallback_relays).await?;
        let progress_reporter = MultiProgress::new();

        for relay in &relays {
            self.client
                .add_relay(relay.as_str())
                .await
                .context("cannot add relay")?;
        }

        let dim = Style::new().color256(247);

        let futures: Vec<_> = relays
            .iter()
            // don't look for events on blaster
            .filter(|r| !r.as_str().contains("nostr.mutinywallet.com"))
            .map(|r| (r.clone(), request.clone()))
            .map(|(relay, request)| async {
                let relay_column_width = request.relay_column_width;

                let pb = if std::env::var("NGITTEST").is_err() {
                    let pb = progress_reporter.add(
                        ProgressBar::new(1)
                            .with_prefix(
                                dim.apply_to(format!(
                                    "{: <relay_column_width$}{}",
                                    "connecting", &relay
                                ))
                                .to_string(),
                            )
                            .with_style(pb_style()?),
                    );
                    pb.enable_steady_tick(Duration::from_millis(300));
                    Some(pb)
                } else {
                    None
                };

                #[allow(clippy::large_futures)]
                match self
                    .fetch_all_from_relay(git_repo_path, relay, request, &pb)
                    .await
                {
                    Err(error) => {
                        if let Some(pb) = pb {
                            pb.set_style(pb_after_style(false));
                            pb.set_prefix(
                                dim.apply_to(format!(
                                    "{: <relay_column_width$}{}",
                                    "error", "&relay"
                                ))
                                .to_string(),
                            );
                            pb.finish_with_message(
                                console::style(
                                    error.to_string().replace("relay pool error:", "error:"),
                                )
                                .for_stderr()
                                .red()
                                .to_string(),
                            );
                        }
                        Err(error)
                    }
                    Ok(res) => {
                        if let Some(pb) = pb {
                            pb.set_style(pb_after_style(true));
                            pb.set_prefix(
                                dim.apply_to(format!(
                                    "{: <relay_column_width$}{}",
                                    if let Some(relay) = &res.relay {
                                        format!("{relay}")
                                    } else {
                                        String::new()
                                    },
                                    if res.to_string().is_empty() {
                                        "no updates".to_string()
                                    } else {
                                        format!("found {res}")
                                    },
                                ))
                                .to_string(),
                            );
                            pb.finish_with_message("");
                        }
                        Ok(res)
                    }
                }
            })
            .collect();

        let relay_reports: Vec<Result<FetchReport>> =
            stream::iter(futures).buffer_unordered(15).collect().await;

        let report = consolidate_fetch_reports(relay_reports);

        if report.to_string().is_empty() {
            println!("no updates found");
        } else {
            println!("fetched updates: {report}");
        }
        Ok(report)
    }

    async fn fetch_all_from_relay(
        &self,
        git_repo_path: &Path,
        relay_url: Url,
        request: FetchRequest,
        // progress_reporter: &MultiProgress,
        pb: &Option<ProgressBar>,
    ) -> Result<FetchReport> {
        let mut fresh_coordinates: HashSet<Coordinate> = HashSet::new();
        for (c, _) in request.repo_coordinates.clone() {
            fresh_coordinates.insert(c);
        }
        let mut fresh_proposal_roots = request.proposals.clone();
        let mut fresh_authors = request.contributor_profiles.clone();

        let mut report = FetchReport {
            relay: Some(relay_url.clone()),
            ..Default::default()
        };

        // let pb = if std::env::var("NGITTEST").is_err() {
        //     let pb = progress_reporter.add(
        //         ProgressBar::new(1)
        //             .with_prefix(format!("{: <11}{}", "connecting", relay_url))
        //             .with_style(pb_style()?),
        //     );
        //     pb.enable_steady_tick(Duration::from_millis(300));
        //     Some(pb)
        // } else {
        //     None
        // };

        self.connect(&relay_url).await?;

        let relay_column_width = request.relay_column_width;

        let dim = Style::new().color256(247);

        loop {
            let filters =
                get_fetch_filters(&fresh_coordinates, &fresh_proposal_roots, &fresh_authors);

            if let Some(pb) = &pb {
                pb.set_prefix(
                    dim.apply_to(format!(
                        "{: <relay_column_width$}{}",
                        &relay_url,
                        if report.to_string().is_empty() {
                            "fetching...".to_string()
                        } else {
                            format!("found {report}")
                        },
                    ))
                    .to_string(),
                );
            }

            fresh_coordinates = HashSet::new();
            fresh_proposal_roots = HashSet::new();
            fresh_authors = HashSet::new();

            let relay = self.client.relay(&relay_url).await?;
            let events: Vec<nostr::Event> = get_events_of(&relay, filters, &None).await?;
            // TODO: try reconcile

            for event in events {
                // TODO existing_events or events in fresh
                process_fetched_event(
                    event,
                    &request,
                    git_repo_path,
                    &mut fresh_coordinates,
                    &mut fresh_proposal_roots,
                    &mut report,
                )
                .await?;
            }

            if fresh_coordinates.is_empty() && fresh_proposal_roots.is_empty() {
                break;
            }
        }
        if let Some(pb) = pb {
            let report_display = format!("{report}");
            pb.set_prefix(
                dim.apply_to(format!(
                    "{: <relay_column_width$}{}",
                    relay_url,
                    if report_display.is_empty() {
                        String::new()
                    } else {
                        format!("found {report_display}")
                    },
                ))
                .to_string(),
            );
        }
        Ok(report)
    }
}

static CONNECTION_TIMEOUT: u64 = 3;
static GET_EVENTS_TIMEOUT: u64 = 7;

async fn get_events_of(
    relay: &nostr_sdk::Relay,
    filters: Vec<nostr::Filter>,
    pb: &Option<ProgressBar>,
) -> Result<Vec<Event>> {
    // relay.reconcile(filter, opts).await?;

    if !relay.is_connected().await {
        #[allow(clippy::large_futures)]
        relay
            .connect(Some(std::time::Duration::from_secs(CONNECTION_TIMEOUT)))
            .await;
    }

    if !relay.is_connected().await {
        bail!("connection timeout");
    } else if let Some(pb) = pb {
        pb.set_prefix(format!("connected  {}", relay.url()));
    }
    let events = relay
        .get_events_of(
            filters,
            // 20 is nostr_sdk default
            std::time::Duration::from_secs(GET_EVENTS_TIMEOUT),
            nostr_sdk::FilterOptions::ExitOnEOSE,
        )
        .await?;
    Ok(events)
}

#[derive(Default)]
pub struct Params {
    pub keys: Option<nostr::Keys>,
    pub fallback_relays: Vec<String>,
    pub more_fallback_relays: Vec<String>,
    pub blaster_relays: Vec<String>,
}

fn get_dedup_events(relay_results: Vec<Result<Vec<nostr::Event>>>) -> Vec<Event> {
    let mut dedup_events: Vec<Event> = vec![];
    for events in relay_results.into_iter().flatten() {
        for event in events {
            if !dedup_events.iter().any(|e| event.id.eq(&e.id)) {
                dedup_events.push(event);
            }
        }
    }
    dedup_events
}

pub async fn sign_event(event_builder: EventBuilder, signer: &NostrSigner) -> Result<nostr::Event> {
    if signer.r#type().eq(&nostr_signer::NostrSignerType::NIP46) {
        let term = console::Term::stderr();
        term.write_line("signing event with remote signer...")?;
        let event = signer
            .sign_event_builder(event_builder)
            .await
            .context("failed to sign event")?;
        term.clear_last_lines(1)?;
        Ok(event)
    } else {
        signer
            .sign_event_builder(event_builder)
            .await
            .context("failed to sign event")
    }
}

pub async fn fetch_public_key(signer: &NostrSigner) -> Result<nostr::PublicKey> {
    let term = console::Term::stderr();
    term.write_line("fetching npub from remote signer...")?;
    let public_key = signer
        .public_key()
        .await
        .context("failed to get npub from remote signer")?;
    term.clear_last_lines(1)?;
    Ok(public_key)
}

fn pb_style() -> Result<ProgressStyle> {
    Ok(
        ProgressStyle::with_template(" {spinner} {prefix} {msg} {timeout_in}")?.with_key(
            "timeout_in",
            |state: &ProgressState, w: &mut dyn Write| {
                if state.elapsed().as_secs() > 3 && state.elapsed().as_secs() < GET_EVENTS_TIMEOUT {
                    let dim = Style::new().color256(247);
                    write!(
                        w,
                        "{}",
                        dim.apply_to(format!(
                            "timeout in {:.1}s",
                            GET_EVENTS_TIMEOUT - state.elapsed().as_secs()
                        ))
                    )
                    .unwrap();
                }
            },
        ),
    )
}

fn pb_after_style(succeed: bool) -> indicatif::ProgressStyle {
    ProgressStyle::with_template(
        format!(
            " {} {}",
            if succeed {
                console::style("✔".to_string())
                    .for_stderr()
                    .green()
                    .to_string()
            } else {
                console::style("✘".to_string())
                    .for_stderr()
                    .red()
                    .to_string()
            },
            "{prefix} {msg}",
        )
        .as_str(),
    )
    .unwrap()
}

async fn get_local_cache_database(git_repo_path: &Path) -> Result<SQLiteDatabase> {
    SQLiteDatabase::open(git_repo_path.join(".git/nostr-cache.sqlite"))
        .await
        .context("cannot open or create nostr cache database at .git/nostr-cache.sqlite")
}

async fn get_global_cache_database(git_repo_path: &Path) -> Result<SQLiteDatabase> {
    SQLiteDatabase::open(if std::env::var("NGITTEST").is_err() {
        create_dir_all(get_dirs()?.config_dir()).context(format!(
            "cannot create cache directory in: {:?}",
            get_dirs()?.config_dir()
        ))?;
        get_dirs()?.config_dir().join("cache.sqlite")
    } else {
        git_repo_path.join(".git/test-global-cache.sqlite")
    })
    .await
    .context("cannot open ngit global nostr cache database")
}

pub async fn get_event_from_cache(
    git_repo_path: &Path,
    filters: Vec<nostr::Filter>,
) -> Result<Vec<nostr::Event>> {
    get_local_cache_database(git_repo_path)
        .await?
        .query(filters.clone(), Order::Asc)
        .await
        .context(
            "cannot execute query on opened git repo nostr cache database .git/nostr-cache.sqlite",
        )
}

pub async fn get_event_from_global_cache(
    git_repo_path: &Path,
    filters: Vec<nostr::Filter>,
) -> Result<Vec<nostr::Event>> {
    get_global_cache_database(git_repo_path)
        .await?
        .query(filters.clone(), Order::Asc)
        .await
        .context("cannot execute query on opened ngit nostr cache database")
}

pub async fn save_event_in_cache(git_repo_path: &Path, event: &nostr::Event) -> Result<bool> {
    get_local_cache_database(git_repo_path)
        .await?
        .save_event(event)
        .await
        .context("cannot save event in local cache")
}

pub async fn save_event_in_global_cache(
    git_repo_path: &Path,
    event: &nostr::Event,
) -> Result<bool> {
    get_global_cache_database(git_repo_path)
        .await?
        .save_event(event)
        .await
        .context("cannot save event in local cache")
}

pub async fn get_repo_ref_from_cache(
    git_repo_path: &Path,
    repo_coordinates: &HashSet<Coordinate>,
) -> Result<RepoRef> {
    let mut maintainers = HashSet::new();
    let mut new_coordinate = false;

    for c in repo_coordinates {
        maintainers.insert(c.public_key);
    }
    let mut repo_events = vec![];
    loop {
        let filter = get_filter_repo_events(repo_coordinates);

        let events = [
            get_event_from_global_cache(git_repo_path, vec![filter.clone()]).await?,
            get_event_from_cache(git_repo_path, vec![filter]).await?,
        ]
        .concat();
        for e in events {
            if let Ok(repo_ref) = RepoRef::try_from(e.clone()) {
                for m in repo_ref.maintainers {
                    if maintainers.insert(m) {
                        new_coordinate = true;
                    }
                }
                repo_events.push(e);
            }
        }
        if !new_coordinate {
            break;
        }
    }
    repo_events.sort_by_key(|e| e.created_at);
    let repo_ref = RepoRef::try_from(
        repo_events
            .first()
            .context("no repo events at specified coordinates")?
            .clone(),
    )?;

    let mut events: HashMap<Coordinate, nostr::Event> = HashMap::new();
    for m in &maintainers {
        if let Some(e) = repo_events.iter().find(|e| e.author().eq(m)) {
            events.insert(
                Coordinate {
                    kind: e.kind,
                    identifier: e.identifier().unwrap().to_string(),
                    public_key: e.author(),
                    relays: vec![],
                },
                e.clone(),
            );
        }
    }

    Ok(RepoRef {
        // use all maintainers from all events found, not just maintainers in the most
        // recent event
        maintainers: maintainers.iter().copied().collect::<Vec<PublicKey>>(),
        events,
        ..repo_ref
    })
}

async fn create_relays_request(
    git_repo_path: &Path,
    repo_coordinates: &HashSet<Coordinate>,
    fallback_relays: HashSet<Url>,
) -> Result<(HashSet<Url>, FetchRequest)> {
    let repo_ref = get_repo_ref_from_cache(git_repo_path, repo_coordinates).await;

    let relays = {
        let mut relays = fallback_relays;
        if let Ok(repo_ref) = &repo_ref {
            for r in &repo_ref.relays {
                if let Ok(url) = Url::parse(r) {
                    relays.insert(url);
                }
            }
        }
        relays
    };

    let relay_column_width = relays
        .iter()
        .reduce(|a, r| {
            if r.to_string()
                .chars()
                .count()
                .gt(&a.to_string().chars().count())
            {
                r
            } else {
                a
            }
        })
        .unwrap()
        .to_string()
        .chars()
        .count()
        + 2;

    let repo_coordinates = if let Ok(repo_ref) = &repo_ref {
        repo_ref.coordinates()
    } else {
        repo_coordinates.clone()
    };

    let proposals: HashSet<EventId> = get_local_cache_database(git_repo_path)
        .await?
        .negentropy_items(
            nostr::Filter::default()
                .kinds(vec![Kind::Custom(PATCH_KIND)])
                .custom_tag(
                    SingleLetterTag::lowercase(nostr_sdk::Alphabet::A),
                    repo_coordinates
                        .iter()
                        .map(std::string::ToString::to_string)
                        .collect::<Vec<String>>(),
                ),
        )
        .await?
        .iter()
        .map(|(id, _)| *id)
        .collect();

    let contributor_profiles = HashSet::new();

    let existing_events: HashSet<EventId> = {
        let mut existing_events: HashSet<EventId> = HashSet::new();
        for filter in get_fetch_filters(&repo_coordinates, &proposals, &contributor_profiles) {
            for (id, _) in get_local_cache_database(git_repo_path)
                .await?
                .negentropy_items(filter)
                .await?
            {
                existing_events.insert(id);
            }
        }
        existing_events
    };
    Ok((
        relays,
        FetchRequest {
            relay_column_width,
            repo_coordinates: if let Ok(repo_ref) = repo_ref {
                repo_ref.coordinates_with_timestamps()
            } else {
                repo_coordinates.iter().map(|c| (c.clone(), None)).collect()
            },
            proposals,
            contributor_profiles,
            existing_events,
        },
    ))
}

async fn process_fetched_event(
    event: nostr::Event,
    request: &FetchRequest,
    git_repo_path: &Path,
    fresh_coordinates: &mut HashSet<Coordinate>,
    fresh_proposal_roots: &mut HashSet<EventId>,
    report: &mut FetchReport,
) -> Result<()> {
    if !request.existing_events.contains(&event.id) {
        save_event_in_cache(git_repo_path, &event).await?;
        if event.kind().as_u16().eq(&REPO_REF_KIND) {
            save_event_in_global_cache(git_repo_path, &event).await?;
            let new_coordinate = !request.repo_coordinates.iter().any(|(c, _)| {
                c.identifier.eq(event.identifier().unwrap()) && c.public_key.eq(&event.pubkey)
            });
            let update_to_existing = !new_coordinate
                && request.repo_coordinates.iter().any(|(c, t)| {
                    c.identifier.eq(event.identifier().unwrap())
                        && c.public_key.eq(&event.pubkey)
                        && if let Some(t) = t {
                            event.created_at.gt(t)
                        } else {
                            false
                        }
                });
            if new_coordinate || update_to_existing {
                let c = Coordinate {
                    kind: event.kind(),
                    public_key: event.author(),
                    identifier: event.identifier().unwrap().to_string(),
                    relays: vec![],
                };
                if new_coordinate {
                    fresh_coordinates.insert(c.clone());
                    report.repo_coordinates.push(c.clone());
                }
                if update_to_existing {
                    report
                        .updated_repo_announcements
                        .push((c, event.created_at));
                }
            }
            // if contains new maintainer
            if let Ok(repo_ref) = &RepoRef::try_from(event.clone()) {
                for m in &repo_ref.maintainers {
                    if !request
                        .repo_coordinates
                        .iter()
                        .any(|(c, _)| c.identifier.eq(&repo_ref.identifier) && m.eq(&c.public_key))
                    {
                        fresh_coordinates.insert(Coordinate {
                            kind: event.kind(),
                            public_key: *m,
                            identifier: repo_ref.identifier.clone(),
                            relays: vec![],
                        });
                    }
                }
            }
        } else if event_is_patch_set_root(&event) {
            fresh_proposal_roots.insert(event.id);
            report.proposals.insert(event.id);
        } else if !event.event_ids().any(|id| report.proposals.contains(id)) {
            if event.kind().as_u16() == PATCH_KIND {
                report.commits.insert(event.id);
            } else if status_kinds().contains(&event.kind()) {
                report.statuses.insert(event.id);
            }
        } else if event.kind().eq(&nostr_sdk::Kind::Metadata) {
            report.contributor_profiles.insert(event.author());
            save_event_in_global_cache(git_repo_path, &event).await?;
        }
    }
    Ok(())
}

fn consolidate_fetch_reports(reports: Vec<Result<FetchReport>>) -> FetchReport {
    let mut report = FetchReport::default();
    for relay_report in reports.into_iter().flatten() {
        for c in relay_report.repo_coordinates {
            if !report.repo_coordinates.iter().any(|e| e.eq(&c)) {
                report.repo_coordinates.push(c);
            }
        }
        for (r, t) in relay_report.updated_repo_announcements {
            if let Some(i) = report
                .updated_repo_announcements
                .iter()
                .position(|(e, _)| e.eq(&r))
            {
                let (_, existing_t) = &report.updated_repo_announcements[i];
                if t.gt(existing_t) {
                    report.updated_repo_announcements[i] = (r, t);
                }
            } else {
                report.updated_repo_announcements.push((r, t));
            }
        }
        for c in relay_report.proposals {
            report.proposals.insert(c);
        }
        for c in relay_report.commits {
            report.commits.insert(c);
        }
        for c in relay_report.statuses {
            report.statuses.insert(c);
        }
    }
    report
}
pub fn get_fetch_filters(
    repo_coordinates: &HashSet<Coordinate>,
    proposal_ids: &HashSet<EventId>,
    required_profiles: &HashSet<PublicKey>,
) -> Vec<nostr::Filter> {
    [
        if repo_coordinates.is_empty() {
            vec![]
        } else {
            vec![
                get_filter_repo_events(repo_coordinates),
                nostr::Filter::default()
                    .kinds(vec![Kind::Custom(PATCH_KIND), Kind::EventDeletion])
                    .custom_tag(
                        SingleLetterTag::lowercase(nostr_sdk::Alphabet::A),
                        repo_coordinates
                            .iter()
                            .map(std::string::ToString::to_string)
                            .collect::<Vec<String>>(),
                    ),
            ]
        },
        if proposal_ids.is_empty() {
            vec![]
        } else {
            vec![
                nostr::Filter::default().events(proposal_ids.clone()).kinds(
                    [
                        vec![Kind::Custom(PATCH_KIND), Kind::EventDeletion],
                        status_kinds(),
                    ]
                    .concat(),
                ),
            ]
        },
        if required_profiles.is_empty() {
            vec![]
        } else {
            vec![
                nostr::Filter::default()
                    .kinds(vec![Kind::Metadata, Kind::RelayList])
                    .authors(required_profiles.clone()),
            ]
        },
    ]
    .concat()
}

pub fn get_filter_repo_events(repo_coordinates: &HashSet<Coordinate>) -> nostr::Filter {
    nostr::Filter::default()
        .kind(Kind::Custom(REPO_REF_KIND))
        .identifiers(
            repo_coordinates
                .iter()
                .map(|c| c.identifier.clone())
                .collect::<Vec<String>>(),
        )
        .authors(
            repo_coordinates
                .iter()
                .map(|c| c.public_key)
                .collect::<Vec<PublicKey>>(),
        )
}

#[derive(Default)]
pub struct FetchReport {
    relay: Option<Url>,
    repo_coordinates: Vec<Coordinate>,
    updated_repo_announcements: Vec<(Coordinate, Timestamp)>,
    proposals: HashSet<EventId>,
    /// commits against existing propoals
    commits: HashSet<EventId>,
    statuses: HashSet<EventId>,
    contributor_profiles: HashSet<PublicKey>,
}

impl Display for FetchReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // report: "1 new maintainer, 1 announcement, 1 proposal, 3 commits, 2 statuses"
        let mut display_items: Vec<String> = vec![];
        if !self.repo_coordinates.is_empty() {
            display_items.push(format!(
                "{} new maintainer{}",
                self.repo_coordinates.len(),
                if self.repo_coordinates.len() == 1 {
                    "s"
                } else {
                    ""
                },
            ));
        }
        if !self.updated_repo_announcements.is_empty() {
            display_items.push(format!(
                "{} announcement update{}",
                self.updated_repo_announcements.len(),
                if self.updated_repo_announcements.len() == 1 {
                    "s"
                } else {
                    ""
                },
            ));
        }
        if !self.proposals.is_empty() {
            display_items.push(format!(
                "{} proposal{}",
                self.proposals.len(),
                if self.proposals.len() == 1 { "s" } else { "" },
            ));
        }
        if !self.commits.is_empty() {
            display_items.push(format!(
                "{} commit{}",
                self.commits.len(),
                if self.commits.len() == 1 { "s" } else { "" },
            ));
        }
        if !self.statuses.is_empty() {
            display_items.push(format!(
                "{} status{}",
                self.statuses.len(),
                if self.statuses.len() == 1 { "es" } else { "" },
            ));
        }
        if !self.contributor_profiles.is_empty() {
            display_items.push(format!(
                "{} contributor profile{}",
                self.contributor_profiles.len(),
                if self.contributor_profiles.len() == 1 {
                    "s"
                } else {
                    ""
                },
            ));
        }
        write!(f, "{}", display_items.join(", "))
    }
}

#[derive(Default, Clone)]
pub struct FetchRequest {
    relay_column_width: usize,
    repo_coordinates: Vec<(Coordinate, Option<Timestamp>)>,
    proposals: HashSet<EventId>,
    contributor_profiles: HashSet<PublicKey>,
    existing_events: HashSet<EventId>,
}
