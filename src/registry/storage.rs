use std::{
    collections::HashMap,
    io::Write,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    sync::{Mutex, Semaphore},
};

use crate::{
    core::{
        color::{Color, ColorExt},
        config::CONFIG,
        constant::CACHE_PATH,
        util::format_bytes,
    },
    error,
    registry::{
        installed::InstalledPackages,
        package::{parse_package_query, ResolvedPackage},
    },
    warn,
};

use super::{
    package::{run::Runner, Package, PackageQuery},
    select_package_variant,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageStorage {
    repository: HashMap<String, RepositoryPackages>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RepositoryPackages {
    #[serde(flatten)]
    pub collection: HashMap<String, HashMap<String, Vec<Package>>>,
}

impl PackageStorage {
    pub fn new() -> Self {
        Self {
            repository: HashMap::new(),
        }
    }

    pub fn add_repository(&mut self, repo_name: &str, packages: RepositoryPackages) {
        self.repository.insert(repo_name.to_owned(), packages);
    }

    pub fn resolve_package(&self, package_name: &str) -> Result<ResolvedPackage> {
        let pkg_query = parse_package_query(package_name);
        let packages = self
            .get_packages(&pkg_query)
            .ok_or_else(|| anyhow::anyhow!("Package {} not found", package_name))?;
        let package = match packages.len() {
            0 => {
                return Err(anyhow::anyhow!(
                    "Is it a fish? Is is a frog? On no, it's a fly."
                ));
            }
            1 => &ResolvedPackage {
                repo_name: packages[0].repo_name.to_owned(),
                package: packages[0].package.to_owned(),
                collection: packages[0].collection.to_owned(),
            },
            _ => select_package_variant(&packages)?,
        };

        Ok(package.to_owned())
    }

    pub async fn install_packages(
        &self,
        package_names: &[String],
        force: bool,
        is_update: bool,
        installed_packages: Arc<Mutex<InstalledPackages>>,
        portable: Option<String>,
        portable_home: Option<String>,
        portable_config: Option<String>,
    ) -> Result<()> {
        let resolved_packages: Result<Vec<ResolvedPackage>> = package_names
            .iter()
            .map(|package_name| self.resolve_package(package_name))
            .collect();
        let resolved_packages = resolved_packages?;

        let installed_count = Arc::new(AtomicU64::new(0));
        if CONFIG.parallel.unwrap_or_default() {
            let semaphore = Arc::new(Semaphore::new(CONFIG.parallel_limit.unwrap_or(2) as usize));
            let mut handles = Vec::new();

            let pkgs_len = resolved_packages.len();
            for (idx, package) in resolved_packages.iter().enumerate() {
                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let package = package.clone();
                let ic = installed_count.clone();
                let installed_packages = installed_packages.clone();
                let portable = portable.clone();
                let portable_home = portable_home.clone();
                let portable_config = portable_config.clone();

                let handle = tokio::spawn(async move {
                    if let Err(e) = package
                        .install(
                            idx,
                            pkgs_len,
                            force,
                            is_update,
                            installed_packages,
                            portable,
                            portable_home,
                            portable_config,
                        )
                        .await
                    {
                        error!("{}", e);
                    } else {
                        ic.fetch_add(1, Ordering::Relaxed);
                    };
                    drop(permit);
                });

                handles.push(handle);
            }

            for handle in handles {
                handle.await?;
            }
        } else {
            for (idx, package) in resolved_packages.iter().enumerate() {
                if let Err(e) = package
                    .install(
                        idx,
                        resolved_packages.len(),
                        force,
                        is_update,
                        installed_packages.clone(),
                        portable.clone(),
                        portable_home.clone(),
                        portable_config.clone(),
                    )
                    .await
                {
                    error!("{}", e);
                } else {
                    installed_count.fetch_add(1, Ordering::Relaxed);
                };
            }
        }
        println!(
            "Installed {}/{} packages",
            installed_count.load(Ordering::Relaxed).color(Color::Blue),
            resolved_packages.len().color(Color::BrightBlue)
        );
        Ok(())
    }

    pub async fn remove_packages(&self, package_names: &[String]) -> Result<()> {
        let resolved_packages: Vec<ResolvedPackage> = package_names
            .iter()
            .filter_map(|package_name| self.resolve_package(package_name).ok())
            .collect();
        for package in resolved_packages {
            package.remove().await?;
        }

        Ok(())
    }

    pub fn list_packages(&self, collection: Option<&str>) -> Vec<ResolvedPackage> {
        self.repository
            .iter()
            .flat_map(|(repo_name, repo_packages)| {
                repo_packages
                    .collection
                    .iter()
                    .filter(|(key, _)| collection.is_none() || Some(key.as_str()) == collection)
                    .flat_map(|(key, collections)| {
                        collections.iter().flat_map(|(_, packages)| {
                            packages.iter().map(|package| ResolvedPackage {
                                repo_name: repo_name.clone(),
                                collection: key.clone(),
                                package: package.clone(),
                            })
                        })
                    })
            })
            .collect()
    }

    pub fn get_packages(&self, query: &PackageQuery) -> Option<Vec<ResolvedPackage>> {
        let pkg_name = query.name.trim();
        let resolved_packages: Vec<ResolvedPackage> = self
            .repository
            .iter()
            .flat_map(|(repo_name, packages)| {
                packages
                    .collection
                    .iter()
                    .filter(|(collection_key, _)| {
                        query.collection.is_none()
                            || Some(collection_key.as_str()) == query.collection.as_deref()
                    })
                    .flat_map(|(collection_key, map)| {
                        map.get(pkg_name).into_iter().flat_map(|pkgs| {
                            pkgs.iter().filter_map(|pkg| {
                                if pkg.name == pkg_name
                                    && (query.variant.is_none()
                                        || pkg.variant.as_ref() == query.variant.as_ref())
                                {
                                    Some(ResolvedPackage {
                                        repo_name: repo_name.to_owned(),
                                        package: pkg.clone(),
                                        collection: collection_key.clone(),
                                    })
                                } else {
                                    None
                                }
                            })
                        })
                    })
            })
            .collect();

        if !resolved_packages.is_empty() {
            Some(resolved_packages)
        } else {
            None
        }
    }

    pub async fn search(&self, query: &str, case_sensitive: bool) -> Vec<ResolvedPackage> {
        let query = parse_package_query(query);
        let pkg_name = if case_sensitive {
            query.name.trim().to_owned()
        } else {
            query.name.trim().to_lowercase()
        };
        let mut resolved_packages: Vec<(u32, Package, String, String)> = Vec::new();

        for (repo_name, packages) in &self.repository {
            for (collection_name, collection_packages) in &packages.collection {
                let pkgs: Vec<(u32, Package, String, String)> = collection_packages
                    .iter()
                    .flat_map(|(_, packages)| {
                        packages.iter().filter_map(|pkg| {
                            let mut score = 0;
                            let found_pkg_name = if case_sensitive {
                                pkg.name.clone()
                            } else {
                                pkg.name.to_lowercase()
                            };

                            if found_pkg_name == pkg_name {
                                score += 2;
                            } else if found_pkg_name.contains(&pkg_name) {
                                score += 1;
                            } else {
                                return None;
                            }
                            if query.variant.is_none()
                                || pkg.variant.as_ref() == query.variant.as_ref()
                            {
                                Some((
                                    score,
                                    pkg.to_owned(),
                                    collection_name.to_owned(),
                                    repo_name.to_owned(),
                                ))
                            } else {
                                None
                            }
                        })
                    })
                    .collect();
                resolved_packages.extend(pkgs);
            }
        }

        resolved_packages.sort_by(|(a, _, _, _), (b, _, _, _)| b.cmp(a));
        resolved_packages
            .into_iter()
            .filter(|(score, _, _, _)| *score > 0)
            .map(|(_, pkg, collection, repo_name)| ResolvedPackage {
                repo_name,
                package: pkg,
                collection,
            })
            .collect()
    }

    pub async fn inspect(&self, package_name: &str) -> Result<()> {
        let resolved_pkg = self.resolve_package(package_name)?;

        let client = reqwest::Client::new();
        let url = resolved_pkg.package.build_log;
        let response = client.get(&url).send().await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Error fetching log from {} [{}]",
                url.color(Color::Blue),
                response.status().color(Color::Red)
            ));
        }

        let content_length = response.content_length().unwrap_or_default();
        if content_length > 1_048_576 {
            warn!(
                "The log file is too large ({}). Do you really want to download and view it (y/N)? ",
                format_bytes(content_length).color(Color::Magenta)
            );

            std::io::stdout().flush()?;
            let mut response = String::new();

            std::io::stdin().read_line(&mut response)?;

            if !response.trim().eq_ignore_ascii_case("y") {
                return Err(anyhow::anyhow!(""));
            }
        }

        println!(
            "Fetching log from {} [{}]",
            url.color(Color::Blue),
            format_bytes(response.content_length().unwrap_or_default()).color(Color::Magenta)
        );

        let mut stream = response.bytes_stream();

        let mut content = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Failed to read chunk")?;
            content.extend_from_slice(&chunk);
        }
        let log_str = String::from_utf8_lossy(&content).replace("\r", "\n");

        println!("\n{}", log_str);

        Ok(())
    }

    pub async fn run(&self, command: &[String]) -> Result<()> {
        fs::create_dir_all(&*CACHE_PATH).await?;

        let package_name = &command[0];
        let args = if command.len() > 1 {
            &command[1..]
        } else {
            &[]
        };
        let runner = if let Ok(resolved_pkg) = self.resolve_package(package_name) {
            let package_path = CACHE_PATH.join(&resolved_pkg.package.bin_name);
            Runner::new(&resolved_pkg, package_path, args)
        } else {
            let query = parse_package_query(package_name);
            let package_path = CACHE_PATH.join(&query.name);
            let mut resolved_pkg = ResolvedPackage::default();
            resolved_pkg.package.name = query.name;
            resolved_pkg.package.variant = query.variant;

            // TODO: check all the repo for package instead of choosing the first
            let base_url = CONFIG
                .repositories
                .iter()
                .find_map(|repo| {
                    if let Some(collection) = &query.collection {
                        repo.sources.get(collection).cloned()
                    } else {
                        repo.sources.values().next().cloned()
                    }
                })
                .ok_or_else(|| anyhow::anyhow!("No repository found for the package"))?;

            resolved_pkg.collection = query.collection.unwrap_or_else(|| {
                CONFIG
                    .repositories
                    .iter()
                    .find_map(|repo| repo.sources.keys().next().cloned())
                    .unwrap_or_default()
            });

            let download_url = format!("{}/{}", base_url, resolved_pkg.package.full_name('/'));
            resolved_pkg.package.download_url = download_url;
            Runner::new(&resolved_pkg, package_path, args)
        };

        runner.execute().await?;

        Ok(())
    }
}
