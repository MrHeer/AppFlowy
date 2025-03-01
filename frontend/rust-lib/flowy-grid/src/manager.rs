use crate::entities::GridLayout;
use crate::services::block_editor::GridBlockRevisionCompress;
use crate::services::grid_editor::{GridRevisionCompress, GridRevisionEditor};
use crate::services::grid_view_manager::make_grid_view_rev_manager;
use crate::services::persistence::block_index::BlockIndexCache;
use crate::services::persistence::kv::GridKVPersistence;
use crate::services::persistence::migration::GridMigration;
use crate::services::persistence::GridDatabase;
use crate::services::tasks::GridTaskScheduler;
use bytes::Bytes;
use dashmap::DashMap;
use flowy_database::ConnectionPool;
use flowy_error::{FlowyError, FlowyResult};
use flowy_grid_data_model::revision::{BuildGridContext, GridRevision, GridViewRevision};
use flowy_revision::disk::{SQLiteGridBlockRevisionPersistence, SQLiteGridRevisionPersistence};
use flowy_revision::{RevisionManager, RevisionPersistence, RevisionWebSocket, SQLiteRevisionSnapshotPersistence};
use flowy_sync::client_grid::{make_grid_block_operations, make_grid_operations, make_grid_view_operations};
use flowy_sync::entities::revision::{RepeatedRevision, Revision};
use std::sync::Arc;
use tokio::sync::RwLock;

pub trait GridUser: Send + Sync {
    fn user_id(&self) -> Result<String, FlowyError>;
    fn token(&self) -> Result<String, FlowyError>;
    fn db_pool(&self) -> Result<Arc<ConnectionPool>, FlowyError>;
}

pub type GridTaskSchedulerRwLock = Arc<RwLock<GridTaskScheduler>>;

pub struct GridManager {
    grid_editors: Arc<DashMap<String, Arc<GridRevisionEditor>>>,
    grid_user: Arc<dyn GridUser>,
    block_index_cache: Arc<BlockIndexCache>,
    #[allow(dead_code)]
    kv_persistence: Arc<GridKVPersistence>,
    task_scheduler: GridTaskSchedulerRwLock,
    migration: GridMigration,
}

impl GridManager {
    pub fn new(
        grid_user: Arc<dyn GridUser>,
        _rev_web_socket: Arc<dyn RevisionWebSocket>,
        database: Arc<dyn GridDatabase>,
    ) -> Self {
        let grid_editors = Arc::new(DashMap::new());
        let kv_persistence = Arc::new(GridKVPersistence::new(database.clone()));
        let block_index_cache = Arc::new(BlockIndexCache::new(database.clone()));
        let task_scheduler = GridTaskScheduler::new();
        let migration = GridMigration::new(grid_user.clone(), database);
        Self {
            grid_editors,
            grid_user,
            kv_persistence,
            block_index_cache,
            task_scheduler,
            migration,
        }
    }

    pub async fn initialize_with_new_user(&self, _user_id: &str, _token: &str) -> FlowyResult<()> {
        Ok(())
    }

    pub async fn initialize(&self, _user_id: &str, _token: &str) -> FlowyResult<()> {
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all, err)]
    pub async fn create_grid<T: AsRef<str>>(&self, grid_id: T, revisions: RepeatedRevision) -> FlowyResult<()> {
        let grid_id = grid_id.as_ref();
        let db_pool = self.grid_user.db_pool()?;
        let rev_manager = self.make_grid_rev_manager(grid_id, db_pool)?;
        let _ = rev_manager.reset_object(revisions).await?;

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all, err)]
    async fn create_grid_view<T: AsRef<str>>(&self, view_id: T, revisions: RepeatedRevision) -> FlowyResult<()> {
        let view_id = view_id.as_ref();
        let rev_manager = make_grid_view_rev_manager(&self.grid_user, view_id).await?;
        let _ = rev_manager.reset_object(revisions).await?;
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all, err)]
    pub async fn create_grid_block<T: AsRef<str>>(&self, block_id: T, revisions: RepeatedRevision) -> FlowyResult<()> {
        let block_id = block_id.as_ref();
        let db_pool = self.grid_user.db_pool()?;
        let rev_manager = self.make_grid_block_rev_manager(block_id, db_pool)?;
        let _ = rev_manager.reset_object(revisions).await?;
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all, err)]
    pub async fn open_grid<T: AsRef<str>>(&self, grid_id: T) -> FlowyResult<Arc<GridRevisionEditor>> {
        let grid_id = grid_id.as_ref();
        let _ = self.migration.run_v1_migration(grid_id).await;
        self.get_or_create_grid_editor(grid_id).await
    }

    #[tracing::instrument(level = "debug", skip_all, fields(grid_id), err)]
    pub async fn close_grid<T: AsRef<str>>(&self, grid_id: T) -> FlowyResult<()> {
        let grid_id = grid_id.as_ref();
        tracing::Span::current().record("grid_id", &grid_id);
        self.grid_editors.remove(grid_id);
        self.task_scheduler.write().await.unregister_handler(grid_id);
        Ok(())
    }

    // #[tracing::instrument(level = "debug", skip(self), err)]
    pub fn get_grid_editor(&self, grid_id: &str) -> FlowyResult<Arc<GridRevisionEditor>> {
        match self.grid_editors.get(grid_id) {
            None => Err(FlowyError::internal().context("Should call open_grid function first")),
            Some(editor) => Ok(editor.clone()),
        }
    }

    async fn get_or_create_grid_editor(&self, grid_id: &str) -> FlowyResult<Arc<GridRevisionEditor>> {
        match self.grid_editors.get(grid_id) {
            None => {
                let db_pool = self.grid_user.db_pool()?;
                let editor = self.make_grid_rev_editor(grid_id, db_pool).await?;

                if self.grid_editors.contains_key(grid_id) {
                    tracing::warn!("Grid:{} already exists in cache", grid_id);
                }
                self.grid_editors.insert(grid_id.to_string(), editor.clone());
                self.task_scheduler.write().await.register_handler(editor.clone());
                Ok(editor)
            }
            Some(editor) => Ok(editor.clone()),
        }
    }

    #[tracing::instrument(level = "trace", skip(self, pool), err)]
    async fn make_grid_rev_editor(
        &self,
        grid_id: &str,
        pool: Arc<ConnectionPool>,
    ) -> Result<Arc<GridRevisionEditor>, FlowyError> {
        let user = self.grid_user.clone();
        let rev_manager = self.make_grid_rev_manager(grid_id, pool.clone())?;
        let grid_editor = GridRevisionEditor::new(
            grid_id,
            user,
            rev_manager,
            self.block_index_cache.clone(),
            self.task_scheduler.clone(),
        )
        .await?;
        Ok(grid_editor)
    }

    pub fn make_grid_rev_manager(&self, grid_id: &str, pool: Arc<ConnectionPool>) -> FlowyResult<RevisionManager> {
        let user_id = self.grid_user.user_id()?;
        let disk_cache = SQLiteGridRevisionPersistence::new(&user_id, pool.clone());
        let rev_persistence = RevisionPersistence::new(&user_id, grid_id, disk_cache);
        let snapshot_persistence = SQLiteRevisionSnapshotPersistence::new(grid_id, pool);
        let rev_compactor = GridRevisionCompress();
        let rev_manager = RevisionManager::new(&user_id, grid_id, rev_persistence, rev_compactor, snapshot_persistence);
        Ok(rev_manager)
    }

    fn make_grid_block_rev_manager(&self, block_id: &str, pool: Arc<ConnectionPool>) -> FlowyResult<RevisionManager> {
        let user_id = self.grid_user.user_id()?;
        let disk_cache = SQLiteGridBlockRevisionPersistence::new(&user_id, pool.clone());
        let rev_persistence = RevisionPersistence::new(&user_id, block_id, disk_cache);
        let rev_compactor = GridBlockRevisionCompress();
        let snapshot_persistence = SQLiteRevisionSnapshotPersistence::new(block_id, pool);
        let rev_manager =
            RevisionManager::new(&user_id, block_id, rev_persistence, rev_compactor, snapshot_persistence);
        Ok(rev_manager)
    }
}

pub async fn make_grid_view_data(
    user_id: &str,
    view_id: &str,
    layout: GridLayout,
    grid_manager: Arc<GridManager>,
    build_context: BuildGridContext,
) -> FlowyResult<Bytes> {
    let BuildGridContext {
        field_revs,
        block_metas,
        blocks,
        grid_view_revision_data,
    } = build_context;

    for block_meta_data in &blocks {
        let block_id = &block_meta_data.block_id;
        // Indexing the block's rows
        block_meta_data.rows.iter().for_each(|row| {
            let _ = grid_manager.block_index_cache.insert(&row.block_id, &row.id);
        });

        // Create grid's block
        let grid_block_delta = make_grid_block_operations(block_meta_data);
        let block_delta_data = grid_block_delta.json_bytes();
        let repeated_revision: RepeatedRevision =
            Revision::initial_revision(user_id, block_id, block_delta_data).into();
        let _ = grid_manager.create_grid_block(&block_id, repeated_revision).await?;
    }

    // Will replace the grid_id with the value returned by the gen_grid_id()
    let grid_id = view_id.to_owned();
    let grid_rev = GridRevision::from_build_context(&grid_id, field_revs, block_metas);

    // Create grid
    let grid_rev_delta = make_grid_operations(&grid_rev);
    let grid_rev_delta_bytes = grid_rev_delta.json_bytes();
    let repeated_revision: RepeatedRevision =
        Revision::initial_revision(user_id, &grid_id, grid_rev_delta_bytes.clone()).into();
    let _ = grid_manager.create_grid(&grid_id, repeated_revision).await?;

    // Create grid view
    let grid_view = if grid_view_revision_data.is_empty() {
        GridViewRevision::new(grid_id, view_id.to_owned(), layout.into())
    } else {
        GridViewRevision::from_json(grid_view_revision_data)?
    };
    let grid_view_delta = make_grid_view_operations(&grid_view);
    let grid_view_delta_bytes = grid_view_delta.json_bytes();
    let repeated_revision: RepeatedRevision =
        Revision::initial_revision(user_id, view_id, grid_view_delta_bytes).into();
    let _ = grid_manager.create_grid_view(view_id, repeated_revision).await?;

    Ok(grid_rev_delta_bytes)
}
