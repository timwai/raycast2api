use super::{error, model::*};
use crate::{error::GatewayError, storage::db::Db};
use diesel::{
    connection::SimpleConnection,
    prelude::*,
    sql_types::{BigInt, Nullable, Text},
};

#[derive(Debug)]
struct StorageFailure;
impl From<diesel::result::Error> for StorageFailure {
    fn from(_: diesel::result::Error) -> Self {
        Self
    }
}
impl From<GatewayError> for StorageFailure {
    fn from(_: GatewayError) -> Self {
        Self
    }
}

#[derive(Clone)]
pub struct Store {
    db: Db,
}
#[derive(QueryableByName)]
struct Payload {
    #[diesel(sql_type=Text)]
    payload: String,
}
#[derive(QueryableByName)]
struct AccountId {
    #[diesel(sql_type=BigInt)]
    id: i64,
}
impl Store {
    pub fn new(db: Db) -> Result<Self, String> {
        db.lock().batch_execute("CREATE TABLE IF NOT EXISTS proxy_subscriptions(id TEXT PRIMARY KEY, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS proxy_nodes(id TEXT PRIMARY KEY, subscription_id TEXT NOT NULL REFERENCES proxy_subscriptions(id) ON DELETE RESTRICT, identity_key TEXT NOT NULL, payload TEXT NOT NULL, UNIQUE(subscription_id,identity_key));
            CREATE TABLE IF NOT EXISTS account_proxy_bindings(account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE, node_id TEXT NOT NULL REFERENCES proxy_nodes(id) ON DELETE RESTRICT, listener_port INTEGER NOT NULL UNIQUE, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS proxy_runtime(id INTEGER PRIMARY KEY CHECK(id=1), generation INTEGER NOT NULL);
            INSERT OR IGNORE INTO proxy_runtime(id,generation) VALUES(1,0);
            CREATE TABLE IF NOT EXISTS proxy_apply_operations(id TEXT PRIMARY KEY, phase TEXT NOT NULL, idempotency_key TEXT UNIQUE, request_hash TEXT, payload TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS proxy_operations_phase ON proxy_apply_operations(phase);
            CREATE TABLE IF NOT EXISTS proxy_idempotency_keys(key TEXT PRIMARY KEY, request_hash TEXT NOT NULL, operation_id TEXT NOT NULL REFERENCES proxy_apply_operations(id) ON DELETE CASCADE);
            INSERT OR IGNORE INTO proxy_idempotency_keys(key,request_hash,operation_id) SELECT idempotency_key,request_hash,id FROM proxy_apply_operations WHERE idempotency_key IS NOT NULL AND request_hash IS NOT NULL;
            CREATE TABLE IF NOT EXISTS proxy_port_reservations(port INTEGER PRIMARY KEY, account_id INTEGER NOT NULL, operation_id TEXT NOT NULL);")
            .map_err(|_| "proxy schema initialization failed".to_string())?;
        #[derive(QueryableByName)]
        struct Violations {
            #[diesel(sql_type=BigInt)]
            count: i64,
        }
        let violations =
            diesel::sql_query("SELECT count(*) AS count FROM pragma_foreign_key_check")
                .get_result::<Violations>(&mut *db.lock())
                .map_err(|_| "proxy foreign key verification failed".to_string())?;
        if violations.count != 0 {
            return Err(
                "database contains foreign key violations; restore a consistent backup".into(),
            );
        }
        // Older versions retained every disappeared node. These records never route
        // traffic; preserve only those still referenced by an account binding.
        db.lock().batch_execute("DELETE FROM proxy_nodes
            WHERE json_extract(payload, '$.presence') = 'missing'
              AND NOT EXISTS (SELECT 1 FROM account_proxy_bindings WHERE node_id = proxy_nodes.id);")
            .map_err(|_| "failed to clean up stale unbound proxy nodes".to_string())?;
        Ok(Self { db })
    }
    pub fn load(&self) -> Result<Snapshot, GatewayError> {
        crate::storage::db::blocking(|| {
            let mut db = self.db.lock();
            #[derive(QueryableByName)]
            struct Generation {
                #[diesel(sql_type=BigInt)]
                generation: i64,
            }
            let generation = diesel::sql_query("SELECT generation FROM proxy_runtime WHERE id=1")
                .get_result::<Generation>(&mut *db)
                .map_err(db_error)?
                .generation as u64;
            let mut state = Snapshot {
                generation,
                ..Default::default()
            };
            for row in diesel::sql_query("SELECT payload FROM proxy_subscriptions")
                .load::<Payload>(&mut *db)
                .map_err(db_error)?
            {
                let value: Subscription = decode(&row.payload)?;
                state.subscriptions.insert(value.id.clone(), value);
            }
            for row in diesel::sql_query("SELECT payload FROM proxy_nodes")
                .load::<Payload>(&mut *db)
                .map_err(db_error)?
            {
                let value: Node = decode(&row.payload)?;
                state.nodes.insert(value.id.clone(), value);
            }
            for row in diesel::sql_query("SELECT payload FROM account_proxy_bindings")
                .load::<Payload>(&mut *db)
                .map_err(db_error)?
            {
                let value: Binding = decode(&row.payload)?;
                state.bindings.insert(value.account_id, value);
            }
            Ok(state)
        })
    }
    #[cfg(test)]
    pub(super) fn database_for_test(&self) -> Db {
        self.db.clone()
    }
    pub fn account_id(&self, email: &str) -> Result<i64, GatewayError> {
        diesel::sql_query("SELECT id FROM accounts WHERE email=?")
            .bind::<Text, _>(email)
            .get_result::<AccountId>(&mut *self.db.lock())
            .optional()
            .map_err(db_error)?
            .map(|x| x.id)
            .ok_or_else(|| GatewayError::NotFound("account does not exist".into()))
    }
    pub fn account_email(&self, id: i64) -> Result<Option<String>, GatewayError> {
        #[derive(QueryableByName)]
        struct Email {
            #[diesel(sql_type=Text)]
            email: String,
        }
        Ok(diesel::sql_query("SELECT email FROM accounts WHERE id=?")
            .bind::<BigInt, _>(id)
            .get_result::<Email>(&mut *self.db.lock())
            .optional()
            .map_err(db_error)?
            .map(|x| x.email))
    }
    pub fn save_operation(&self, op: &Operation) -> Result<(), GatewayError> {
        diesel::sql_query("INSERT INTO proxy_apply_operations(id,phase,idempotency_key,request_hash,payload) VALUES(?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET phase=excluded.phase,payload=excluded.payload")
            .bind::<Text,_>(&op.id).bind::<Text,_>(&op.phase)
            .bind::<Nullable<Text>,_>(op.idempotency_key.as_deref()).bind::<Nullable<Text>,_>(op.request_hash.as_deref())
            .bind::<Text,_>(encode(op)?).execute(&mut *self.db.lock()).map_err(db_error)?;
        Ok(())
    }
    pub fn operation(&self, id: &str) -> Result<Operation, GatewayError> {
        let row = diesel::sql_query("SELECT payload FROM proxy_apply_operations WHERE id=?")
            .bind::<Text, _>(id)
            .get_result::<Payload>(&mut *self.db.lock())
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| GatewayError::NotFound("proxy operation not found".into()))?;
        decode(&row.payload)
    }
    pub fn pending(&self) -> Result<Vec<Operation>, GatewayError> {
        diesel::sql_query("SELECT payload FROM proxy_apply_operations WHERE phase IN ('queued','preparing','prepared','runtime_applied','recovery_required') ORDER BY rowid")
            .load::<Payload>(&mut *self.db.lock()).map_err(db_error)?.iter().map(|x|decode(&x.payload)).collect()
    }
    pub fn idempotent(
        &self,
        key: &str,
        request_hash: &str,
    ) -> Result<Option<Operation>, GatewayError> {
        #[derive(QueryableByName)]
        struct Existing {
            #[diesel(sql_type=Text)]
            payload: String,
            #[diesel(sql_type=Text)]
            request_hash: String,
        }
        let existing=diesel::sql_query("SELECT o.payload,k.request_hash FROM proxy_idempotency_keys k JOIN proxy_apply_operations o ON o.id=k.operation_id WHERE k.key=?")
            .bind::<Text, _>(key)
            .get_result::<Existing>(&mut *self.db.lock())
            .optional()
            .map_err(db_error)?;
        match existing {
            Some(existing) if existing.request_hash != request_hash => Err(GatewayError::Conflict(
                "idempotency key was used for another request".into(),
            )),
            Some(existing) => Ok(Some(decode(&existing.payload)?)),
            None => Ok(None),
        }
    }
    pub fn insert_operation(
        &self,
        op: &Operation,
        key: Option<&str>,
        request_hash: &str,
    ) -> Result<(), GatewayError> {
        self.db.lock().immediate_transaction::<_,StorageFailure,_>(|db|{
            diesel::sql_query("INSERT INTO proxy_apply_operations(id,phase,payload) VALUES(?,?,?)")
                .bind::<Text,_>(&op.id).bind::<Text,_>(&op.phase).bind::<Text,_>(encode(op)?).execute(db)?;
            if let Some(key)=key{diesel::sql_query("INSERT INTO proxy_idempotency_keys(key,request_hash,operation_id) VALUES(?,?,?)")
                .bind::<Text,_>(key).bind::<Text,_>(request_hash).bind::<Text,_>(&op.id).execute(db)?;}
            Ok(())
        }).map_err(|_|error("proxy_storage_failed","could not persist proxy operation and idempotency key"))
    }
    pub fn remember_key(
        &self,
        key: &str,
        request_hash: &str,
        operation: &str,
    ) -> Result<(), GatewayError> {
        diesel::sql_query(
            "INSERT INTO proxy_idempotency_keys(key,request_hash,operation_id) VALUES(?,?,?)",
        )
        .bind::<Text, _>(key)
        .bind::<Text, _>(request_hash)
        .bind::<Text, _>(operation)
        .execute(&mut *self.db.lock())
        .map_err(db_error)?;
        Ok(())
    }
    pub fn persist_schedules(&self, state: &Snapshot) -> Result<(), GatewayError> {
        self.db
            .lock()
            .immediate_transaction::<_, StorageFailure, _>(|db| {
                for sub in state.subscriptions.values() {
                    diesel::sql_query("UPDATE proxy_subscriptions SET payload=? WHERE id=?")
                        .bind::<Text, _>(encode(sub)?)
                        .bind::<Text, _>(&sub.id)
                        .execute(db)?;
                }
                Ok(())
            })
            .map_err(|_| {
                error(
                    "proxy_storage_failed",
                    "could not persist subscription schedule",
                )
            })
    }
    pub fn release_operation_reservations(&self, operation: &str) -> Result<(), GatewayError> {
        diesel::sql_query("DELETE FROM proxy_port_reservations WHERE operation_id=?")
            .bind::<Text, _>(operation)
            .execute(&mut *self.db.lock())
            .map_err(db_error)?;
        Ok(())
    }
    pub fn commit(
        &self,
        state: &Snapshot,
        op: &Operation,
        delete_ids: &[i64],
    ) -> Result<(), GatewayError> {
        crate::storage::db::blocking(|| {
            self.db.lock().immediate_transaction::<_,StorageFailure,_>(|db|{
            db.batch_execute("DELETE FROM account_proxy_bindings; DELETE FROM proxy_nodes; DELETE FROM proxy_subscriptions;").map_err(db_error)?;
            for id in delete_ids { diesel::sql_query("DELETE FROM accounts WHERE id=?").bind::<BigInt,_>(id).execute(db).map_err(db_error)?; }
            for sub in state.subscriptions.values(){
                diesel::sql_query("INSERT INTO proxy_subscriptions(id,payload) VALUES(?,?)").bind::<Text,_>(&sub.id).bind::<Text,_>(encode(sub)?).execute(db).map_err(db_error)?;
            }
            for node in state.nodes.values(){
                diesel::sql_query("INSERT INTO proxy_nodes(id,subscription_id,identity_key,payload) VALUES(?,?,?,?)")
                    .bind::<Text,_>(&node.id).bind::<Text,_>(&node.subscription_id).bind::<Text,_>(&node.identity_key).bind::<Text,_>(encode(node)?).execute(db).map_err(db_error)?;
            }
            for binding in state.bindings.values(){
                diesel::sql_query("INSERT INTO account_proxy_bindings(account_id,node_id,listener_port,payload) VALUES(?,?,?,?)")
                    .bind::<BigInt,_>(binding.account_id).bind::<Text,_>(&binding.node_id).bind::<BigInt,_>(binding.listener_port as i64).bind::<Text,_>(encode(binding)?).execute(db).map_err(db_error)?;
            }
            diesel::sql_query("UPDATE proxy_runtime SET generation=? WHERE id=1").bind::<BigInt,_>(state.generation as i64).execute(db).map_err(db_error)?;
            diesel::sql_query("UPDATE proxy_apply_operations SET phase=?,payload=? WHERE id=?")
                .bind::<Text,_>(&op.phase).bind::<Text,_>(encode(op)?).bind::<Text,_>(&op.id).execute(db).map_err(db_error)?;
            db.batch_execute("DELETE FROM proxy_port_reservations;").map_err(db_error)?;
            Ok(())
        }).map_err(|_| error("proxy_storage_failed","proxy database transaction failed"))
        })
    }
    pub fn reserve(&self, port: u16, account: i64, operation: &str) -> Result<(), GatewayError> {
        diesel::sql_query(
            "INSERT INTO proxy_port_reservations(port,account_id,operation_id) VALUES(?,?,?)",
        )
        .bind::<BigInt, _>(port as i64)
        .bind::<BigInt, _>(account)
        .bind::<Text, _>(operation)
        .execute(&mut *self.db.lock())
        .map_err(db_error)?;
        Ok(())
    }
    pub fn release_reservations(&self) -> Result<(), GatewayError> {
        self.db
            .lock()
            .batch_execute("DELETE FROM proxy_port_reservations;")
            .map_err(db_error)
    }
}
fn encode<T: serde::Serialize>(value: &T) -> Result<String, GatewayError> {
    serde_json::to_string(value)
        .map_err(|_| error("proxy_storage_failed", "proxy serialization failed"))
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, GatewayError> {
    serde_json::from_str(value)
        .map_err(|_| error("proxy_storage_failed", "invalid stored proxy data"))
}
fn db_error(_: diesel::result::Error) -> GatewayError {
    error("proxy_storage_failed", "proxy database operation failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy_manager::model::test_data::*;
    #[test]
    fn startup_cleans_legacy_unbound_tombstones_without_changing_bindings_or_generation() {
        let db = Db::open(":memory:").unwrap();
        let accounts = crate::account::store::AccountStore::new(db.clone()).unwrap();
        accounts
            .upsert(
                &serde_json::from_value(serde_json::json!({"user":{"email":"bound@fixture"},
            "device_id":"1".repeat(64),"access_token":"rca_fixture"}))
                .unwrap(),
            )
            .unwrap();
        let store = Store::new(db.clone()).unwrap();
        let account = store.account_id("bound@fixture").unwrap();
        let mut snapshot = Snapshot {
            generation: 7,
            ..Default::default()
        };
        snapshot
            .subscriptions
            .insert("sub".into(), subscription("sub"));
        for node in [
            node("removed", "sub", "missing"),
            node("bound", "sub", "missing"),
            node("current", "sub", "present"),
        ] {
            snapshot.nodes.insert(node.id.clone(), node);
        }
        snapshot.bindings.insert(account, binding(account, "bound"));
        let operation = Operation {
            id: id(),
            phase: "committed".into(),
            code: None,
            message: None,
            generation: Some(7),
            revision: None,
            created_at: 1,
            updated_at: 1,
            candidate: None,
            action_ciphertext: None,
            idempotency_key: None,
            request_hash: None,
            resource: None,
            result: None,
        };
        store.save_operation(&operation).unwrap();
        store.commit(&snapshot, &operation, &[]).unwrap();
        let restored = Store::new(db).unwrap().load().unwrap();
        assert_eq!(restored.generation, 7);
        assert_eq!(restored.nodes.len(), 2);
        assert!(!restored.nodes.contains_key("removed"));
        assert_eq!(restored.nodes["bound"].presence, "missing");
        assert_eq!(restored.bindings[&account].node_id, "bound");
        assert_eq!(restored.subscriptions["sub"].revision, 1);
    }
}
