use crate::webserver::ErrorWithStatus;
use crate::webserver::{make_placeholder, Database};
use crate::{AppState, TEMPLATES_DIR};
use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::any::{AnyKind, AnyStatement, AnyTypeInfo};
use sqlx::postgres::types::PgTimeTz;
use sqlx::{Postgres, Statement, Type};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

pub(crate) struct FileSystem {
    local_root: PathBuf,
    db_fs_queries: Option<DbFsQueries>,
}

impl FileSystem {
    pub async fn init(local_root: impl Into<PathBuf>, db: &Database) -> Self {
        Self {
            local_root: local_root.into(),
            db_fs_queries: match DbFsQueries::init(db).await {
                Ok(q) => Some(q),
                Err(e) => {
                    log::debug!(
                        "Using local filesystem only, could not initialize on-database filesystem. \
                        You can host sql files directly in your database by creating the following table: \n\
                        {} \n\
                        The error while trying to use the database file system is: {e:#}",
                        DbFsQueries::get_create_table_sql(db.connection.any_kind())
                    );
                    None
                }
            },
        }
    }

    pub async fn modified_since(
        &self,
        app_state: &AppState,
        path: &Path,
        since: DateTime<Utc>,
        priviledged: bool,
    ) -> anyhow::Result<bool> {
        let local_path = self.safe_local_path(app_state, path, priviledged)?;
        let local_result = file_modified_since_local(&local_path, since).await;
        match (local_result, &self.db_fs_queries) {
            (Ok(modified), _) => Ok(modified),
            (Err(e), Some(db_fs)) if e.kind() == ErrorKind::NotFound => {
                // no local file, try the database
                db_fs
                    .file_modified_since_in_db(app_state, path, since)
                    .await
            }
            (Err(e), _) => {
                Err(e).with_context(|| format!("Unable to read local file metadata for {path:?}"))
            }
        }
    }

    pub async fn read_to_string(
        &self,
        app_state: &AppState,
        path: &Path,
        priviledged: bool,
    ) -> anyhow::Result<String> {
        let bytes = self.read_file(app_state, path, priviledged).await?;
        String::from_utf8(bytes)
            .with_context(|| format!("The file at {path:?} contains invalid UTF8 characters"))
    }

    /**
     * Priviledged files are the ones that are in sqlpage's config directory.
     */
    pub async fn read_file(
        &self,
        app_state: &AppState,
        path: &Path,
        priviledged: bool,
    ) -> anyhow::Result<Vec<u8>> {
        let local_path = self.safe_local_path(app_state, path, priviledged)?;
        log::debug!("Reading file {path:?} from {local_path:?}");
        let local_result = tokio::fs::read(&local_path).await;
        match (local_result, &self.db_fs_queries) {
            (Ok(f), _) => Ok(f),
            (Err(e), Some(db_fs)) if e.kind() == ErrorKind::NotFound => {
                // no local file, try the database
                db_fs.read_file(app_state, path.as_ref()).await
            }
            (Err(e), None) if e.kind() == ErrorKind::NotFound => Err(ErrorWithStatus {
                status: actix_web::http::StatusCode::NOT_FOUND,
            }
            .into()),
            (Err(e), _) => Err(e).with_context(|| format!("Unable to read local file {path:?}")),
        }
    }

    fn safe_local_path(
        &self,
        app_state: &AppState,
        path: &Path,
        priviledged: bool,
    ) -> anyhow::Result<PathBuf> {
        if priviledged {
            // Templates requests are always made to the static TEMPLATES_DIR, because this is where they are stored in the database
            // but when serving them from the filesystem, we need to serve them from the `SQLPAGE_CONFIGURATION_DIRECTORY/templates` directory
            if let Ok(template_path) = path.strip_prefix(TEMPLATES_DIR) {
                let normalized = [
                    &app_state.config.configuration_directory,
                    Path::new("templates"),
                    template_path,
                ]
                .iter()
                .collect();
                log::trace!("Normalizing template path {path:?} to {normalized:?}");
                return Ok(normalized);
            }
        } else {
            for (i, component) in path.components().enumerate() {
                if let Component::Normal(c) = component {
                    if i == 0 && c.eq_ignore_ascii_case("sqlpage") {
                        anyhow::bail!(ErrorWithStatus {
                            status: actix_web::http::StatusCode::FORBIDDEN,
                        });
                    }
                } else {
                    anyhow::bail!(
                    "Unsupported path: {path:?}. Path component '{component:?}' is not allowed."
                );
                }
            }
        }
        Ok(self.local_root.join(path))
    }
}

async fn file_modified_since_local(path: &Path, since: DateTime<Utc>) -> tokio::io::Result<bool> {
    tokio::fs::metadata(path)
        .await
        .and_then(|m| m.modified())
        .map(|modified_at| DateTime::<Utc>::from(modified_at) > since)
}

pub(crate) struct DbFsQueries {
    was_modified: AnyStatement<'static>,
    read_file: AnyStatement<'static>,
}

impl DbFsQueries {
    fn get_create_table_sql(db_kind: AnyKind) -> &'static str {
        match db_kind {
            AnyKind::Mssql => "CREATE TABLE sqlpage_files(path NVARCHAR(255) NOT NULL PRIMARY KEY, contents VARBINARY(MAX), last_modified DATETIME2(3) NOT NULL DEFAULT CURRENT_TIMESTAMP);",
            AnyKind::Postgres => "CREATE TABLE IF NOT EXISTS sqlpage_files(path VARCHAR(255) NOT NULL PRIMARY KEY, contents BYTEA, last_modified TIMESTAMP DEFAULT CURRENT_TIMESTAMP);",
            _ => "CREATE TABLE IF NOT EXISTS sqlpage_files(path VARCHAR(255) NOT NULL PRIMARY KEY, contents BLOB, last_modified TIMESTAMP DEFAULT CURRENT_TIMESTAMP);",
        }
    }

    async fn init(db: &Database) -> anyhow::Result<Self> {
        log::debug!("Initializing database filesystem queries");
        let db_kind = db.connection.any_kind();
        Ok(Self {
            was_modified: Self::make_was_modified_query(db, db_kind).await?,
            read_file: Self::make_read_file_query(db, db_kind).await?,
        })
    }

    async fn make_was_modified_query(
        db: &Database,
        db_kind: AnyKind,
    ) -> anyhow::Result<AnyStatement<'static>> {
        let was_modified_query = format!(
            "SELECT 1 from sqlpage_files WHERE last_modified >= {} AND path = {}",
            make_placeholder(db_kind, 1),
            make_placeholder(db_kind, 2)
        );
        let param_types: &[AnyTypeInfo; 2] = &[
            PgTimeTz::type_info().into(),
            <str as Type<Postgres>>::type_info().into(),
        ];
        db.prepare_with(&was_modified_query, param_types).await
    }

    async fn make_read_file_query(
        db: &Database,
        db_kind: AnyKind,
    ) -> anyhow::Result<AnyStatement<'static>> {
        let was_modified_query = format!(
            "SELECT contents from sqlpage_files WHERE path = {}",
            make_placeholder(db_kind, 1),
        );
        let param_types: &[AnyTypeInfo; 1] = &[<str as Type<Postgres>>::type_info().into()];
        db.prepare_with(&was_modified_query, param_types).await
    }

    async fn file_modified_since_in_db(
        &self,
        app_state: &AppState,
        path: &Path,
        since: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let query = self
            .was_modified
            .query_as::<(bool,)>()
            .bind(since)
            .bind(path.display().to_string());
        log::trace!(
            "Checking if file {path:?} was modified since {since} by executing query: \n\
            {}\n\
            with parameters: {:?}",
            self.was_modified.sql(),
            (since, path)
        );
        query
            .fetch_optional(&app_state.db.connection)
            .await
            .map(|modified| modified.is_some())
            .with_context(|| {
                format!("Unable to check when {path:?} was last modified in the database")
            })
    }

    async fn read_file(&self, app_state: &AppState, path: &Path) -> anyhow::Result<Vec<u8>> {
        log::debug!("Reading file {} from the database", path.display());
        self.read_file
            .query_as::<(Vec<u8>,)>()
            .bind(path.display().to_string())
            .fetch_optional(&app_state.db.connection)
            .await
            .map_err(anyhow::Error::from)
            .and_then(|modified| {
                if let Some((modified,)) = modified {
                    Ok(modified)
                } else {
                    Err(ErrorWithStatus {
                        status: actix_web::http::StatusCode::NOT_FOUND,
                    }
                    .into())
                }
            })
            .with_context(|| format!("Unable to read {path:?} from the database"))
    }
}

#[actix_web::test]
async fn test_sql_file_read_utf8() -> anyhow::Result<()> {
    use crate::app_config;
    use sqlx::Executor;
    let config = app_config::tests::test_config();
    let state = AppState::init(&config).await?;
    let create_table_sql = DbFsQueries::get_create_table_sql(state.db.connection.any_kind());
    state
        .db
        .connection
        .execute(format!("DROP TABLE IF EXISTS sqlpage_files; {create_table_sql}").as_str())
        .await?;

    let db_kind = state.db.connection.any_kind();
    let insert_sql = format!(
        "INSERT INTO sqlpage_files(path, contents) VALUES ({}, {})",
        make_placeholder(db_kind, 1),
        make_placeholder(db_kind, 2)
    );
    sqlx::query(&insert_sql)
        .bind("unit test file.txt")
        .bind("Héllö world! 😀".as_bytes())
        .execute(&state.db.connection)
        .await?;

    let fs = FileSystem::init("/", &state.db).await;
    let actual = fs
        .read_to_string(&state, "unit test file.txt".as_ref(), false)
        .await?;
    assert_eq!(actual, "Héllö world! 😀");

    let one_hour_ago = Utc::now() - chrono::Duration::hours(1);
    let one_hour_future = Utc::now() + chrono::Duration::hours(1);

    let was_modified = fs
        .modified_since(&state, "unit test file.txt".as_ref(), one_hour_ago, false)
        .await?;
    assert!(was_modified, "File should be modified since one hour ago");

    let was_modified = fs
        .modified_since(
            &state,
            "unit test file.txt".as_ref(),
            one_hour_future,
            false,
        )
        .await?;
    assert!(
        !was_modified,
        "File should not be modified since one hour in the future"
    );

    Ok(())
}
