use sqlx::{query, query_as, FromRow, Transaction};

pub mod model;

use model::{
    BigId, Count, CreateUser, Member, Permission, PermissionType, RefreshToken, UpdateWorkspace,
    User, UserCred, UserInWorkspace, UserLogin, UserWithNonce, Workspace, WorkspaceDetail,
    WorkspaceType, WorkspaceWithPermission,
};

#[derive(FromRow)]
struct PermissionQuery {
    #[sqlx(rename = "type")]
    type_: PermissionType,
}

#[cfg(feature = "mysql")]
type DBPool = sqlx::PgPool;
#[cfg(feature = "sqlite")]
type DBPool = sqlx::SqlitePool;

#[cfg(feature = "mysql")]
type DBType = sqlx::Postgres;
#[cfg(feature = "sqlite")]
type DBType = sqlx::Sqlite;

pub struct DBContext {
    pub db: DBPool,
}

impl DBContext {
    pub async fn new(db_env: String) -> DBContext {
        let db = DBPool::connect(&db_env)
            .await
            .expect("wrong database URL");
        let db_context = Self { db };
        db_context.init_db().await;
        db_context
    }

    pub async fn init_db(&self) {
        let stmt = "CREATE TABLE IF NOT EXISTS users (
            id SERIAL PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT NOT NULL,
            avatar_url TEXT,
            token_nonce SMALLINT DEFAULT 0,
            password TEXT,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            UNIQUE (email)
        );";
        query(&stmt)
            .execute(&self.db)
            .await
            .expect("create table users failed");

        let stmt = "CREATE TABLE IF NOT EXISTS google_users (
            id SERIAL PRIMARY KEY,
            user_id INTEGER REFERENCES users(id),
            google_id TEXT NOT NULL,
            UNIQUE (google_id)
        );";
        query(&stmt)
            .execute(&self.db)
            .await
            .expect("create table google_users failed");

        let stmt = "CREATE TABLE IF NOT EXISTS workspaces (
            id BIGSERIAL PRIMARY KEY,
            public BOOL NOT NULL,
            type SMALLINT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );";
        query(&stmt)
            .execute(&self.db)
            .await
            .expect("create table workspaces failed");

        let stmt = "CREATE TABLE IF NOT EXISTS permissions (
            id BIGSERIAL PRIMARY KEY,
            workspace_id BIGINT REFERENCES workspaces(id) ON DELETE CASCADE,
            user_id INTEGER REFERENCES users(id),
            user_email TEXT,
            type SMALLINT NOT NULL,
            accepted BOOL DEFAULT False,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            UNIQUE (workspace_id, user_id),
            UNIQUE (workspace_id, user_email)
        );";
        query(&stmt)
            .execute(&self.db)
            .await
            .expect("create table permissions failed");
    }

    pub async fn get_user_by_email(&self, email: &str) -> sqlx::Result<Option<User>> {
        let stmt = "SELECT id, name, email, avatar_url, created_at FROM users WHERE email = $1";

        query_as::<_, User>(stmt)
            .bind(email)
            .fetch_optional(&self.db)
            .await
    }

    pub async fn get_workspace_owner(&self, workspace_id: i64) -> sqlx::Result<User> {
        let stmt = format!(
            "SELECT
                users.id, users.name, users.email, users.avatar_url, users.created_at
            FROM permissions
            INNER JOIN users
                ON permissions.user_id = users.id
            WHERE workspace_id = $1 AND type = {}",
            PermissionType::Owner as i16
        );
        query_as::<_, User>(&stmt)
            .bind(workspace_id)
            .fetch_one(&self.db)
            .await
    }

    pub async fn user_login(&self, login: UserLogin) -> sqlx::Result<Option<UserWithNonce>> {
        let stmt = "SELECT 
            id, name, email, avatar_url, token_nonce, created_at
        FROM users
        WHERE email = $1 AND password = $2";

        query_as::<_, UserWithNonce>(stmt)
            .bind(login.email)
            .bind(login.password)
            .fetch_optional(&self.db)
            .await
    }

    pub async fn refresh_token(&self, token: RefreshToken) -> sqlx::Result<Option<UserWithNonce>> {
        let stmt = "SELECT 
            id, name, email, avatar_url, token_nonce, created_at
        FROM users
        WHERE id = $1 AND token_nonce = $2";

        query_as::<_, UserWithNonce>(stmt)
            .bind(token.user_id)
            .bind(token.token_nonce)
            .fetch_optional(&self.db)
            .await
    }

    pub async fn verify_refresh_token(&self, token: &RefreshToken) -> sqlx::Result<bool> {
        let stmt = "SELECT True
        FROM users
        WHERE id = $1 AND token_nonce = $2";

        query(stmt)
            .bind(token.user_id)
            .bind(token.token_nonce)
            .fetch_optional(&self.db)
            .await
            .map(|r| r.is_some())
    }

    pub async fn update_cred(
        trx: &mut Transaction<'static, DBType>,
        user_id: i32,
        user_email: &str,
    ) -> sqlx::Result<()> {
        let update_cred = "UPDATE permissions
        SET user_id = $1,
            user_email = NULL
        WHERE user_email = $2";

        query(update_cred)
            .bind(user_id)
            .bind(user_email)
            .execute(&mut *trx)
            .await?;

        Ok(())
    }

    pub async fn create_user(&self, user: CreateUser) -> sqlx::Result<Option<User>> {
        let mut trx = self.db.begin().await?;
        let create_user = "INSERT INTO users 
            (name, password, email, avatar_url) 
            VALUES ($1, $2, $3, $4)
        ON CONFLICT email DO NOTHING
        RETURNING id, name, email, avatar_url, created_at";

        let Some(user) = query_as::<_, User>(create_user)
            .bind(user.name)
            .bind(user.password)
            .bind(user.email)
            .bind(user.avatar_url)
            .fetch_optional(&mut trx)
            .await? else {
                return Ok(None)
        };

        Self::create_workspace(&mut trx, user.id, WorkspaceType::Private).await?;

        Self::update_cred(&mut trx, user.id, &user.email).await?;

        trx.commit().await?;

        Ok(Some(user))
    }

    pub async fn get_workspace_by_id(
        &self,
        workspace_id: i64,
    ) -> sqlx::Result<Option<WorkspaceDetail>> {
        let get_workspace = "SELECT id, public, type, created_at FROM workspaces WHERE id = $1;";

        let workspace = query_as::<_, Workspace>(&get_workspace)
            .bind(workspace_id)
            .fetch_optional(&self.db)
            .await?;

        let workspace = match workspace {
            Some(workspace) if workspace.type_ == WorkspaceType::Private => {
                return Ok(Some(WorkspaceDetail {
                    owner: None,
                    member_count: 0,
                    workspace,
                }))
            }
            Some(ws) => ws,
            None => return Ok(None),
        };

        let owner = self.get_workspace_owner(workspace_id).await?;

        let get_member_count = "SELECT COUNT(permissions.id)
            FROM permissions
            WHERE workspace_id = $1 AND accepted = True";

        let member_count = query_as::<_, Count>(get_member_count)
            .bind(workspace_id)
            .fetch_one(&self.db)
            .await?
            .count;

        Ok(Some(WorkspaceDetail {
            owner: Some(owner),
            member_count,
            workspace,
        }))
    }

    pub async fn create_workspace(
        trx: &mut Transaction<'static, DBType>,
        user_id: i32,
        ws_type: WorkspaceType,
    ) -> sqlx::Result<Workspace> {
        let create_workspace = format!(
            "INSERT INTO workspaces (public, type) VALUES (false, $1) 
            RETURNING id, public, created_at, type;",
        );

        let workspace = query_as::<_, Workspace>(&create_workspace)
            .bind(ws_type as i16)
            .fetch_one(&mut *trx)
            .await?;

        let create_permission = format!(
            "INSERT INTO permissions
                (user_id, workspace_id, type, accepted)
            VALUES ($1, $2, {}, True);",
            PermissionType::Owner as i16
        );

        query(&create_permission)
            .bind(user_id)
            .bind(workspace.id)
            .execute(&mut *trx)
            .await?;

        Ok(workspace)
    }

    pub async fn create_normal_workspace(&self, user_id: i32) -> sqlx::Result<Workspace> {
        let mut trx = self.db.begin().await?;
        let workspace = Self::create_workspace(&mut trx, user_id, WorkspaceType::Normal).await?;

        trx.commit().await?;

        Ok(workspace)
    }

    pub async fn update_workspace(
        &self,
        workspace_id: i64,
        data: UpdateWorkspace,
    ) -> sqlx::Result<Option<Workspace>> {
        let update_workspace = format!(
            "UPDATE workspaces
                SET public = $1
            WHERE id = $2 AND type = {}
            RETURNING id, public, type, created_at;",
            WorkspaceType::Normal as i16
        );

        query_as::<_, Workspace>(&update_workspace)
            .bind(data.public)
            .bind(workspace_id)
            .fetch_optional(&self.db)
            .await
    }

    pub async fn delete_workspace(&self, workspace_id: i64) -> sqlx::Result<bool> {
        let delete_workspace = format!(
            "DELETE FROM workspaces CASCADE
            WHERE id = $1 AND type = {}",
            WorkspaceType::Normal as i16
        );

        query(&delete_workspace)
            .bind(workspace_id)
            .execute(&self.db)
            .await
            .map(|q| q.rows_affected() != 0)
    }

    pub async fn get_user_workspaces(
        &self,
        user_id: i32,
    ) -> sqlx::Result<Vec<WorkspaceWithPermission>> {
        let stmt = "SELECT 
            workspaces.id, workspaces.public, workspaces.created_at, workspaces.type,
            permissions.type as permission
        FROM permissions
        INNER JOIN workspaces
          ON permissions.workspace_id = workspaces.id
        WHERE user_id = $1";

        query_as::<_, WorkspaceWithPermission>(&stmt)
            .bind(user_id)
            .fetch_all(&self.db)
            .await
    }

    pub async fn get_workspace_members(&self, workspace_id: i64) -> sqlx::Result<Vec<Member>> {
        let stmt = "SELECT 
            permissions.id, permissions.type, permissions.user_email,
            permissions.accepted, permissions.created_at,
            users.id as user_id, users.name as user_name, users.email as user_table_email, users.avatar_url,
            users.created_at as user_created_at
        FROM permissions
        LEFT JOIN users
            ON users.id = permissions.user_id
        WHERE workspace_id = $1";

        query_as::<_, Member>(stmt)
            .bind(workspace_id)
            .fetch_all(&self.db)
            .await
    }

    pub async fn get_permission(
        &self,
        user_id: i32,
        workspace_id: i64,
    ) -> sqlx::Result<Option<PermissionType>> {
        let stmt = "SELECT type FROM permissions WHERE user_id = $1 AND workspace_id = $2";

        query_as::<_, PermissionQuery>(&stmt)
            .bind(user_id)
            .bind(workspace_id)
            .fetch_optional(&self.db)
            .await
            .map(|p| p.map(|p| p.type_))
    }

    pub async fn get_permission_by_permission_id(
        &self,
        user_id: i32,
        permission_id: i64,
    ) -> sqlx::Result<Option<PermissionType>> {
        let stmt = "SELECT type FROM permissions
        WHERE
            user_id = $1
        AND
            workspace_id = (SELECT workspace_id FROM permissions WHERE permissions.id = $2)
        ";

        query_as::<_, PermissionQuery>(&stmt)
            .bind(user_id)
            .bind(permission_id)
            .fetch_optional(&self.db)
            .await
            .map(|p| p.map(|p| p.type_))
    }

    pub async fn can_read_workspace(&self, user_id: i32, workspace_id: i64) -> sqlx::Result<bool> {
        let stmt = "SELECT FROM permissions 
            WHERE user_id = $1
                AND workspace_id = $2
                OR (SELECT True FROM workspaces WHERE id = $2 AND public = True)";

        query(&stmt)
            .bind(user_id)
            .bind(workspace_id)
            .fetch_optional(&self.db)
            .await
            .map(|p| p.is_some())
    }

    pub async fn create_permission(
        &self,
        email: &str,
        workspace_id: i64,
        permission_type: PermissionType,
    ) -> sqlx::Result<Option<(i64, UserCred)>> {
        let user = self.get_user_by_email(email).await?;

        let stmt = format!(
            "INSERT INTO permissions (user_id, user_email, workspace_id, type)
            SELECT $1, $2, $3, $4
            FROM workspaces
                WHERE workspaces.type = {} AND workspaces.id = $3
            ON CONFLICT DO NOTHING
            RETURNING id",
            WorkspaceType::Normal as i16
        );

        let query = query_as::<_, BigId>(&stmt);

        let (query, user) = match user {
            Some(user) => (
                query.bind(user.id).bind::<Option<String>>(None),
                UserCred::Registered(user),
            ),
            None => (
                query.bind::<Option<i32>>(None).bind(email),
                UserCred::UnRegistered {
                    email: email.to_owned(),
                },
            ),
        };

        let id = query
            .bind(workspace_id)
            .bind(permission_type as i16)
            .fetch_optional(&self.db)
            .await?;

        Ok(if let Some(id) = id {
            Some((id.id, user))
        } else {
            None
        })
    }

    pub async fn accept_permission(&self, permission_id: i64) -> sqlx::Result<Option<Permission>> {
        let stmt = "UPDATE permissions
                SET accepted = True
            WHERE id = $1
            RETURNING id, user_id, user_email, workspace_id, type, accepted, created_at";

        query_as::<_, Permission>(&stmt)
            .bind(permission_id)
            .fetch_optional(&self.db)
            .await
    }

    pub async fn delete_permission(&self, permission_id: i64) -> sqlx::Result<bool> {
        let stmt = "DELETE FROM permissions WHERE id = $1";

        query(&stmt)
            .bind(permission_id)
            .execute(&self.db)
            .await
            .map(|q| q.rows_affected() != 0)
    }

    pub async fn delete_permission_by_query(
        &self,
        user_id: i32,
        workspace_id: i64,
    ) -> sqlx::Result<bool> {
        let stmt = format!(
            "DELETE FROM permissions
            WHERE user_id = $1 AND workspace_id = $2 AND type != {}",
            PermissionType::Owner as i16
        );

        query(&stmt)
            .bind(user_id)
            .bind(workspace_id)
            .execute(&self.db)
            .await
            .map(|q| q.rows_affected() != 0)
    }

    pub async fn get_user_in_workspace_by_email(
        &self,
        workspace_id: i64,
        email: &str,
    ) -> sqlx::Result<UserInWorkspace> {
        let stmt = "SELECT 
            id, name, email, avatar_url, token_nonce, created_at
        FROM users";

        let user = query_as::<_, User>(stmt)
            .bind(workspace_id)
            .fetch_optional(&self.db)
            .await?;

        Ok(if let Some(user) = user {
            let stmt = "SELECT True FROM permissions WHERE workspace_id = $1 AND user_id = $2";

            let in_workspace = query(stmt)
                .bind(workspace_id)
                .bind(user.id)
                .fetch_optional(&self.db)
                .await?
                .is_some();

            UserInWorkspace {
                user: UserCred::Registered(user),
                in_workspace,
            }
        } else {
            let stmt = "SELECT True FROM permissions WHERE workspace_id = $1 AND user_email = $2";

            let in_workspace = query_as::<_, User>(stmt)
                .bind(workspace_id)
                .bind(email)
                .fetch_optional(&self.db)
                .await?
                .is_some();

            UserInWorkspace {
                user: UserCred::UnRegistered {
                    email: email.to_string(),
                },
                in_workspace,
            }
        })
    }
}
