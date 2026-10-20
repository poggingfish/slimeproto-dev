use super::*;
use axum::extract::{RawQuery, State};
use std::{path::PathBuf, sync::Arc};

struct Fixture {
    state: AppState,
    path: PathBuf,
    owners: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("slimeproto-refs-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&path).unwrap();
        let environment = unsafe {
            heed::EnvOpenOptions::new()
                .map_size(16 * 1024 * 1024)
                .max_dbs(1)
                .open(&path)
                .unwrap()
        };
        let mut txn = environment.write_txn().unwrap();
        let database = environment
            .create_database(&mut txn, Some("slimeproto"))
            .unwrap();
        txn.commit().unwrap();
        let state = Arc::new(crate::ServerState {
            environment,
            database,
            config: crate::config::Config::default(),
            limits: crate::rate_limit::RateLimiter::default(),
        });
        let mut owners = Vec::new();
        for seed in [[1; 32], [2; 32]] {
            let owner = crate::registration::store_account(&state, &seed, "test-password")
                .unwrap()
                .account_id;
            for listed in [false, true] {
                let mut collection = Collection {
                    uuid: generate_uuid_v7(),
                    name: if listed { "listed" } else { "unlisted" }.into(),
                    author: owner.clone(),
                    listed,
                    signature: String::new(),
                };
                let mut txn = state.environment.write_txn().unwrap();
                collection.signature =
                    crate::identity::sign_collection(&state, &txn, &collection).unwrap();
                database
                    .put(
                        &mut txn,
                        format!("collection:{}", collection.uuid).as_bytes(),
                        &serde_json::to_vec(&collection).unwrap(),
                    )
                    .unwrap();
                database
                    .put(
                        &mut txn,
                        &crate::names::collection_key(&owner, &collection.name).unwrap(),
                        collection.uuid.as_bytes(),
                    )
                    .unwrap();
                txn.commit().unwrap();
            }
            owners.push(owner);
        }
        Self {
            state,
            path,
            owners,
        }
    }

    fn create(
        &self,
        owner: usize,
        collection: &str,
        refs: &[&str],
    ) -> Result<Document, ActionError> {
        let input = CreateDocument {
            collection: collection.into(),
            name: None,
            kind: "note".into(),
            content: data(&[("text", "hello")]),
            refs: refs.iter().map(|uuid| (*uuid).into()).collect(),
        };
        let retry = RetryInput {
            key: generate_uuid_v7(),
            fingerprint: [0; 32],
        };
        let outcome = create_document(
            &self.state,
            &AuthenticatedUser {
                user_id: self.owners[owner].clone(),
            },
            input,
            "Write1",
            Some(retry),
            self.state.environment.write_txn().unwrap(),
        )?;
        let WriteOutcome::Created(created) = outcome else {
            panic!("expected a new document")
        };
        let Value::String(uuid) = &created.reply.data["uuid"] else {
            panic!("expected a UUID")
        };
        let txn = self.state.environment.read_txn().unwrap();
        Ok(serde_json::from_slice(
            self.state
                .database
                .get(&txn, format!("document:{uuid}").as_bytes())
                .unwrap()
                .unwrap(),
        )
        .unwrap())
    }

    fn snapshot(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let txn = self.state.environment.read_txn().unwrap();
        self.state
            .database
            .iter(&txn)
            .unwrap()
            .map(|row| {
                let (key, value) = row.unwrap();
                (key.to_vec(), value.to_vec())
            })
            .collect()
    }

    async fn read(&self, event: &str, document: &str) -> serde_json::Value {
        let response = crate::spa::spa_get_handler(
            State(self.state.clone()),
            RawQuery(Some(format!("event={event}&id=Read1&document={document}"))),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Linux permits removing these files while the environment is still open.
        std::fs::remove_dir_all(&self.path).unwrap();
    }
}

#[tokio::test]
async fn unlisted_targets_are_rejected_without_changing_storage() {
    let fixture = Fixture::new();
    for target_owner in [0, 1] {
        let listed = fixture.create(target_owner, "listed", &[]).unwrap();
        let unlisted = fixture.create(target_owner, "unlisted", &[]).unwrap();
        for source in ["listed", "unlisted"] {
            let before = fixture.snapshot();
            let error = match fixture.create(0, source, &[&listed.uuid, &unlisted.uuid]) {
                Err(error) => error,
                Ok(_) => panic!("unlisted targets must be rejected"),
            };
            assert_eq!(
                fixture.snapshot(),
                before,
                "rejected references must not change documents, indexes, quotas, sequences, or retry records"
            );
            let response = error.into_response("Write1");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(response["event"], "create_document_err");
            assert_eq!(
                response["data"]["fakywaky"],
                "referenced documents must belong to listed collections"
            );
        }
    }
    let before = fixture.snapshot();
    assert!(matches!(
        fixture.create(0, "listed", &[&generate_uuid_v7()]),
        Err(ActionError::ReferenceNotFound)
    ));
    assert_eq!(fixture.snapshot(), before);
}

#[tokio::test]
async fn unlisted_sources_keep_refs_but_stay_out_of_reverse_lookups_after_migration() {
    let fixture = Fixture::new();
    let target = fixture.create(1, "listed", &[]).unwrap();
    let hidden = fixture.create(0, "unlisted", &[&target.uuid]).unwrap();
    let listed = fixture.create(0, "listed", &[&target.uuid]).unwrap();
    assert_eq!(hidden.refs, vec![target.uuid.clone()]);

    for migrate in [false, true] {
        if migrate {
            let mut txn = fixture.state.environment.write_txn().unwrap();
            fixture
                .state
                .database
                .delete(
                    &mut txn,
                    &sequence_index(
                        format!("document_referrer:{}:", target.uuid).as_bytes(),
                        listed.seq,
                    ),
                )
                .unwrap();
            txn.commit().unwrap();
            crate::spa::migrate_reference_index(&fixture.state.environment, fixture.state.database)
                .unwrap();
        }
        let refs = fixture.read("get_document_refs", &hidden.uuid).await;
        assert_eq!(refs["data"]["refs"]["0"], target.uuid);
        for action in ["list_document_referrers", "list_document_referrers_content"] {
            let response = fixture.read(action, &target.uuid).await;
            let documents = response["data"]["documents"].as_object().unwrap();
            assert_eq!(documents.len(), 1);
            if action == "list_document_referrers" {
                assert_eq!(documents["0"], listed.uuid);
            } else {
                assert_eq!(documents["0"]["uuid"], listed.uuid);
            }
        }
        let txn = fixture.state.environment.read_txn().unwrap();
        assert!(
            fixture
                .state
                .database
                .get(
                    &txn,
                    &sequence_index(
                        format!("document_referrer:{}:", target.uuid).as_bytes(),
                        hidden.seq
                    )
                )
                .unwrap()
                .is_none()
        );
    }
}
