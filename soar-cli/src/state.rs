use std::{
    fs::{self, File},
    path::PathBuf,
    sync::{Arc, Mutex, RwLockReadGuard},
};

use rusqlite::Connection;
use soar_core::{
    config::{get_config, Config},
    constants::CORE_MIGRATIONS,
    database::{connection::Database, migration::MigrationManager},
    metadata::fetch_metadata,
    SoarResult,
};

#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    config: RwLockReadGuard<'static, Config>,
    repo_db: Database,
    core_db: Database,
}

impl AppState {
    pub async fn new() -> SoarResult<Self> {
        let config = get_config();

        Self::init_repo_dbs(&config).await?;
        let repo_db = Self::create_repo_db(&config)?;
        let core_db = Self::create_core_db(&config)?;

        Ok(Self {
            inner: Arc::new(AppStateInner {
                config,
                repo_db,
                core_db,
            }),
        })
    }

    async fn init_repo_dbs(config: &RwLockReadGuard<'_, Config>) -> SoarResult<()> {
        for repo in &config.repositories {
            let db_file = repo.get_path()?.join("metadata.db");
            if !db_file.exists() {
                fs::create_dir_all(repo.get_path()?)?;
                File::create(&db_file)?;
            }
            fetch_metadata(repo.clone()).await?;
        }
        Ok(())
    }

    fn create_repo_db(config: &RwLockReadGuard<'_, Config>) -> SoarResult<Database> {
        let repo_paths: Vec<PathBuf> = config
            .repositories
            .iter()
            .map(|r| r.get_path().unwrap().join("metadata.db"))
            .collect();

        Database::new_multi(repo_paths.as_ref())
    }

    fn create_core_db(config: &RwLockReadGuard<'_, Config>) -> SoarResult<Database> {
        let core_db_file = config.get_db_path()?.join("soar.db");
        if !core_db_file.exists() {
            File::create(&core_db_file)?;
        }

        let conn = Connection::open(&core_db_file)?;
        let mut manager = MigrationManager::new(conn)?;
        manager.migrate_from_dir(CORE_MIGRATIONS)?;
        Database::new(&core_db_file)
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn repo_db(&self) -> &Arc<Mutex<Connection>> {
        &self.inner.repo_db.conn
    }

    pub fn core_db(&self) -> &Arc<Mutex<Connection>> {
        &self.inner.core_db.conn
    }
}
