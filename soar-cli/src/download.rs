use std::sync::Arc;

use indicatif::HumanBytes;
use regex::Regex;
use serde::Deserialize;
use soar_core::SoarResult;
use soar_dl::{
    downloader::{DownloadOptions, DownloadState, Downloader},
    github::{Github, GithubAsset, GithubRelease},
    gitlab::{Gitlab, GitlabAsset, GitlabRelease},
    platform::{
        PlatformDownloadOptions, PlatformUrl, Release, ReleaseAsset, ReleaseHandler,
        ReleasePlatform,
    },
};
use tracing::{error, info};

use crate::{
    progress::{self, create_progress_bar},
    utils::interactive_ask,
};

pub struct DownloadContext {
    regex_patterns: Option<Vec<String>>,
    match_keywords: Option<Vec<String>>,
    exclude_keywords: Option<Vec<String>>,
    output: Option<String>,
    yes: bool,
    progress_callback: Arc<dyn Fn(DownloadState) + Send + Sync>,
}

pub async fn download(
    links: Vec<String>,
    github: Vec<String>,
    gitlab: Vec<String>,
    ghcr: Vec<String>,
    regex_patterns: Option<Vec<String>>,
    match_keywords: Option<Vec<String>>,
    exclude_keywords: Option<Vec<String>>,
    output: Option<String>,
    yes: bool,
) -> SoarResult<()> {
    let progress_bar = create_progress_bar();
    let progress_callback = Arc::new(move |state| progress::handle_progress(state, &progress_bar));

    let ctx = DownloadContext {
        regex_patterns: regex_patterns.clone(),
        match_keywords: match_keywords.clone(),
        exclude_keywords: exclude_keywords.clone(),
        output: output.clone(),
        yes,
        progress_callback: progress_callback.clone(),
    };

    handle_direct_downloads(&ctx, links, output.clone(), progress_callback.clone()).await?;

    if !github.is_empty() {
        handle_github_downloads(&ctx, github).await?;
    }

    if !gitlab.is_empty() {
        handle_gitlab_downloads(&ctx, gitlab).await?;
    }

    if !ghcr.is_empty() {
        handle_oci_downloads(ghcr, output.clone(), progress_callback.clone()).await?;
    }

    Ok(())
}

pub async fn handle_direct_downloads(
    ctx: &DownloadContext,
    links: Vec<String>,
    output: Option<String>,
    progress_callback: Arc<dyn Fn(DownloadState) + Send + Sync>,
) -> SoarResult<()> {
    let downloader = Downloader::default();

    for link in &links {
        match PlatformUrl::parse(link) {
            Ok(PlatformUrl::DirectUrl(url)) => {
                info!("Downloading using direct link: {}", url);

                let options = DownloadOptions {
                    url: link.clone(),
                    output_path: output.clone(),
                    progress_callback: Some(progress_callback.clone()),
                };
                let _ = downloader
                    .download(options)
                    .await
                    .map_err(|e| eprintln!("{}", e));
            }
            Ok(PlatformUrl::Github(project)) => {
                info!("Detected GitHub URL, processing as GitHub release");
                let handler = ReleaseHandler::<Github>::new();
                if let Err(e) = handle_platform_download::<Github, GithubRelease, GithubAsset>(
                    ctx, &handler, &project,
                )
                .await
                {
                    eprintln!("{}", e);
                }
            }
            Ok(PlatformUrl::Gitlab(project)) => {
                info!("Detected GitLab URL, processing as GitLab release");
                let handler = ReleaseHandler::<Gitlab>::new();
                if let Err(e) = handle_platform_download::<Gitlab, GitlabRelease, GitlabAsset>(
                    ctx, &handler, &project,
                )
                .await
                {
                    eprintln!("{}", e);
                }
            }
            Ok(PlatformUrl::Oci(url)) => {
                info!("Downloading using OCI reference: {}", url);

                let options = DownloadOptions {
                    url: link.clone(),
                    output_path: output.clone(),
                    progress_callback: Some(progress_callback.clone()),
                };
                let _ = downloader
                    .download_oci(options)
                    .await
                    .map_err(|e| eprintln!("{}", e));
            }
            Err(err) => eprintln!("Error parsing URL '{}' : {}", link, err),
        };
    }

    Ok(())
}

pub async fn handle_oci_downloads(
    references: Vec<String>,
    output: Option<String>,
    progress_callback: Arc<dyn Fn(DownloadState) + Send + Sync>,
) -> SoarResult<()> {
    let downloader = Downloader::default();

    for reference in &references {
        let options = DownloadOptions {
            url: reference.clone(),
            output_path: output.clone(),
            progress_callback: Some(progress_callback.clone()),
        };

        info!("Downloading using OCI reference: {}", reference);
        let _ = downloader
            .download_oci(options)
            .await
            .map_err(|e| eprintln!("{}", e));
    }
    Ok(())
}

fn create_platform_options(ctx: &DownloadContext, tag: Option<String>) -> PlatformDownloadOptions {
    let asset_regexes = ctx
        .regex_patterns
        .clone()
        .map(|patterns| {
            patterns
                .iter()
                .map(|pattern| Regex::new(pattern))
                .collect::<Result<Vec<Regex>, regex::Error>>()
        })
        .transpose()
        .unwrap()
        .unwrap_or_default();

    PlatformDownloadOptions {
        output_path: ctx.output.clone(),
        progress_callback: Some(ctx.progress_callback.clone()),
        tag,
        regex_patterns: asset_regexes,
        match_keywords: ctx.match_keywords.clone().unwrap_or_default(),
        exclude_keywords: ctx.exclude_keywords.clone().unwrap_or_default(),
        exact_case: false,
    }
}

async fn handle_platform_download<P: ReleasePlatform, R, A>(
    ctx: &DownloadContext,
    handler: &ReleaseHandler<P>,
    project: &str,
) -> SoarResult<()>
where
    R: Release<A> + for<'de> Deserialize<'de>,
    A: ReleaseAsset + Clone,
{
    let (project, tag) = match project.trim().split_once('@') {
        Some((proj, tag)) if !tag.trim().is_empty() => (proj, Some(tag.trim())),
        _ => (project.trim_end_matches('@'), None),
    };

    let options = create_platform_options(&ctx, tag.map(String::from));
    let releases = handler.fetch_releases::<R>(project).await?;
    let assets = handler.filter_releases(&releases, &options).await?;

    let selected_asset = if assets.len() == 1 || ctx.yes {
        assets[0].clone()
    } else {
        select_asset(&assets)?
    };
    handler.download(&selected_asset, options.clone()).await?;
    Ok(())
}

pub async fn handle_github_downloads(
    ctx: &DownloadContext,
    projects: Vec<String>,
) -> SoarResult<()> {
    let handler = ReleaseHandler::<Github>::new();
    for project in &projects {
        info!("Fetching releases from GitHub: {}", project);
        if let Err(e) =
            handle_platform_download::<_, GithubRelease, _>(ctx, &handler, project).await
        {
            eprintln!("{}", e);
        }
    }
    Ok(())
}

pub async fn handle_gitlab_downloads(
    ctx: &DownloadContext,
    projects: Vec<String>,
) -> SoarResult<()> {
    let handler = ReleaseHandler::<Gitlab>::new();
    for project in &projects {
        info!("Fetching releases from GitLab: {}", project);
        if let Err(e) =
            handle_platform_download::<_, GitlabRelease, _>(ctx, &handler, project).await
        {
            eprintln!("{}", e);
        }
    }
    Ok(())
}

fn select_asset<A>(assets: &[A]) -> SoarResult<A>
where
    A: Clone,
    A: ReleaseAsset,
{
    info!("\nAvailable assets:");
    for (i, asset) in assets.iter().enumerate() {
        let size = asset
            .size()
            .map(|s| format!(" ({})", HumanBytes(s)))
            .unwrap_or_default();
        info!("{}. {}{}", i + 1, asset.name(), size);
    }

    loop {
        let max = assets.len();
        let response = interactive_ask(&format!("Select an asset (1-{max}): "))?;
        match response.parse::<usize>() {
            Ok(n) if n > 0 && n <= max => return Ok(assets[n - 1].clone()),
            _ => error!("Invalid selection, please try again."),
        }
    }
}
