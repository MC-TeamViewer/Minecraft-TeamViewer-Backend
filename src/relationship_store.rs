use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};
use unicode_normalization::UnicodeNormalization;

use crate::proto::teamviewer::v1::{
    AffiliationEdge, AffiliationGroup, AffiliationRelationKind, PlayerDirectoryEntry,
    PlayerIdentityRecord, PlayerRelationEvidence, PlayerRelationKind, PlayerRelationResult,
    RelationshipGraphHead, RelationshipGraphSnapshotChunk,
};

const DEFAULT_STALE_AFTER_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Clone)]
pub struct RelationshipStore {
    db: SqlitePool,
}

impl RelationshipStore {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn initialize(&self) -> Result<()> {
        for query in [
            "CREATE TABLE IF NOT EXISTS relationship_datasets (realm_id TEXT NOT NULL, dataset_id TEXT NOT NULL, format TEXT NOT NULL, coverage INTEGER NOT NULL, stale_after_seconds INTEGER NOT NULL, revision TEXT NOT NULL, digest BLOB NOT NULL, health INTEGER NOT NULL, observed_at INTEGER NOT NULL, generated_at INTEGER NOT NULL, PRIMARY KEY (realm_id, dataset_id))",
            "CREATE TABLE IF NOT EXISTS relationship_players (realm_id TEXT NOT NULL, dataset_id TEXT NOT NULL, player_id TEXT NOT NULL, uuid TEXT, name TEXT NOT NULL, aliases_json TEXT NOT NULL, affiliation_ids_json TEXT NOT NULL, observed_at INTEGER NOT NULL, PRIMARY KEY (realm_id, dataset_id, player_id))",
            "CREATE TABLE IF NOT EXISTS relationship_affiliations (realm_id TEXT NOT NULL, dataset_id TEXT NOT NULL, affiliation_id TEXT NOT NULL, kind INTEGER NOT NULL, name TEXT NOT NULL, parent_id TEXT, color_rgb INTEGER, PRIMARY KEY (realm_id, dataset_id, affiliation_id))",
            "CREATE TABLE IF NOT EXISTS relationship_edges (realm_id TEXT NOT NULL, dataset_id TEXT NOT NULL, from_id TEXT NOT NULL, to_id TEXT NOT NULL, relation INTEGER NOT NULL, PRIMARY KEY (realm_id, dataset_id, from_id, to_id, relation))",
        ] {
            sqlx::query(query).execute(&self.db).await?;
        }
        // Added after the initial table definition for existing installations.
        let _ = sqlx::query(
            "ALTER TABLE relationship_players ADD COLUMN normalized_name TEXT NOT NULL DEFAULT ''",
        )
        .execute(&self.db)
        .await;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_relationship_players_name ON relationship_players (realm_id, dataset_id, normalized_name)").execute(&self.db).await?;
        Ok(())
    }

    pub async fn upsert_chunk(
        &self,
        descriptor: &crate::proto::teamviewer::v1::ExternalDatasetDescriptor,
        chunk: &RelationshipGraphSnapshotChunk,
        observed_at: i64,
    ) -> Result<()> {
        if chunk.chunk_index == 0 {
            let mut tx = self.db.begin().await?;
            for table in [
                "relationship_players",
                "relationship_affiliations",
                "relationship_edges",
            ] {
                sqlx::query(&format!(
                    "DELETE FROM {table} WHERE realm_id = ? AND dataset_id = ?"
                ))
                .bind(&descriptor.realm_id)
                .bind(&descriptor.dataset_id)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
        }
        self.upsert_rows(
            descriptor,
            chunk
                .head
                .as_ref()
                .context("relationship chunk missing head")?,
            &chunk.players,
            &chunk.affiliations,
            &chunk.edges,
            observed_at,
        )
        .await
    }

    async fn upsert_rows(
        &self,
        descriptor: &crate::proto::teamviewer::v1::ExternalDatasetDescriptor,
        head: &RelationshipGraphHead,
        players: &[PlayerIdentityRecord],
        affiliations: &[AffiliationGroup],
        edges: &[AffiliationEdge],
        observed_at: i64,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        for player in players {
            sqlx::query("INSERT INTO relationship_players (realm_id,dataset_id,player_id,uuid,name,aliases_json,affiliation_ids_json,observed_at,normalized_name) VALUES (?,?,?,?,?,?,?,?,?) ON CONFLICT(realm_id,dataset_id,player_id) DO UPDATE SET uuid=excluded.uuid,name=excluded.name,aliases_json=excluded.aliases_json,affiliation_ids_json=excluded.affiliation_ids_json,observed_at=excluded.observed_at,normalized_name=excluded.normalized_name")
                .bind(&descriptor.realm_id).bind(&descriptor.dataset_id).bind(&player.player_id).bind(&player.uuid).bind(&player.name).bind(serde_json::to_string(&player.aliases)?).bind(serde_json::to_string(&player.affiliation_ids)?).bind(observed_at).bind(normalize(&player.name)).execute(&mut *tx).await?;
        }
        for group in affiliations {
            sqlx::query("INSERT INTO relationship_affiliations (realm_id,dataset_id,affiliation_id,kind,name,parent_id,color_rgb) VALUES (?,?,?,?,?,?,?) ON CONFLICT(realm_id,dataset_id,affiliation_id) DO UPDATE SET kind=excluded.kind,name=excluded.name,parent_id=excluded.parent_id,color_rgb=excluded.color_rgb")
                .bind(&descriptor.realm_id).bind(&descriptor.dataset_id).bind(&group.affiliation_id).bind(group.kind).bind(&group.name).bind(&group.parent_affiliation_id).bind(group.color_rgb.map(i64::from)).execute(&mut *tx).await?;
        }
        for edge in edges {
            sqlx::query("INSERT OR REPLACE INTO relationship_edges (realm_id,dataset_id,from_id,to_id,relation) VALUES (?,?,?,?,?)").bind(&descriptor.realm_id).bind(&descriptor.dataset_id).bind(&edge.from_affiliation_id).bind(&edge.to_affiliation_id).bind(edge.relation).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO relationship_datasets (realm_id,dataset_id,format,coverage,stale_after_seconds,revision,digest,health,observed_at,generated_at) VALUES (?,?,?,?,?,?,?,?,?,?) ON CONFLICT(realm_id,dataset_id) DO UPDATE SET format=excluded.format,coverage=excluded.coverage,stale_after_seconds=excluded.stale_after_seconds,revision=excluded.revision,digest=excluded.digest,health=excluded.health,observed_at=excluded.observed_at,generated_at=excluded.generated_at")
            .bind(&descriptor.realm_id).bind(&descriptor.dataset_id).bind(&descriptor.format).bind(descriptor.coverage).bind(i64::from(descriptor.stale_after_seconds)).bind(&head.revision).bind(&head.digest_sha256).bind(2_i32).bind(observed_at).bind(head.generated_at_utc_ms).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_health(
        &self,
        descriptor: &crate::proto::teamviewer::v1::ExternalDatasetDescriptor,
        health: i32,
        observed_at: i64,
    ) -> Result<()> {
        sqlx::query("UPDATE relationship_datasets SET health = ?, observed_at = ? WHERE realm_id = ? AND dataset_id = ?")
            .bind(health).bind(observed_at).bind(&descriptor.realm_id).bind(&descriptor.dataset_id).execute(&self.db).await?;
        Ok(())
    }

    pub async fn lookup(
        &self,
        realm: &str,
        dataset: Option<&str>,
        selectors: &[crate::proto::teamviewer::v1::PlayerSelector],
        now: i64,
    ) -> Result<Vec<Vec<PlayerDirectoryEntry>>> {
        let dataset = self.dataset_id(realm, dataset).await?;
        let mut result = Vec::new();
        for selector in selectors {
            let mut query = String::from(
                "SELECT player_id,uuid,name,affiliation_ids_json,observed_at FROM relationship_players WHERE realm_id = ? AND dataset_id = ?",
            );
            let value = match &selector.selector {
                Some(crate::proto::teamviewer::v1::player_selector::Selector::PlayerId(v)) => {
                    query.push_str(" AND player_id = ?");
                    Some(v.clone())
                }
                Some(crate::proto::teamviewer::v1::player_selector::Selector::Uuid(v)) => {
                    query.push_str(" AND lower(uuid) = lower(?)");
                    Some(v.clone())
                }
                Some(crate::proto::teamviewer::v1::player_selector::Selector::Name(v)) => {
                    query.push_str(" AND normalized_name = ?");
                    Some(normalize(v))
                }
                None => None,
            };
            let mut q = sqlx::query(&query).bind(realm).bind(&dataset);
            if let Some(value) = value {
                q = q.bind(value);
            }
            let rows = q.fetch_all(&self.db).await?;
            result.push(self.rows_to_entries(realm, &dataset, rows, now).await?);
        }
        Ok(result)
    }

    pub async fn relations(
        &self,
        realm: &str,
        dataset: Option<&str>,
        subject: &crate::proto::teamviewer::v1::PlayerSelector,
        targets: &[crate::proto::teamviewer::v1::PlayerSelector],
        now: i64,
    ) -> Result<(Vec<PlayerDirectoryEntry>, Vec<PlayerRelationResult>)> {
        let subjects = self
            .lookup(realm, dataset, std::slice::from_ref(subject), now)
            .await?
            .remove(0);
        if subjects.len() != 1 {
            return Ok((subjects, Vec::new()));
        }
        let subject = &subjects[0];
        let target_entries = if targets.is_empty() {
            self.lookup_all(realm, dataset, now).await?
        } else {
            self.lookup(realm, dataset, targets, now)
                .await?
                .into_iter()
                .flatten()
                .collect()
        };
        let mut results = Vec::new();
        for target in target_entries {
            let relation = self.relation_for(realm, dataset, subject, &target).await?;
            results.push(relation);
        }
        Ok((subjects, results))
    }

    async fn lookup_all(
        &self,
        realm: &str,
        dataset: Option<&str>,
        now: i64,
    ) -> Result<Vec<PlayerDirectoryEntry>> {
        let dataset = self.dataset_id(realm, dataset).await?;
        let rows = sqlx::query("SELECT player_id,uuid,name,affiliation_ids_json,observed_at FROM relationship_players WHERE realm_id = ? AND dataset_id = ? ORDER BY player_id").bind(realm).bind(&dataset).fetch_all(&self.db).await?;
        self.rows_to_entries(realm, &dataset, rows, now).await
    }

    async fn rows_to_entries(
        &self,
        realm: &str,
        dataset: &str,
        rows: Vec<sqlx::sqlite::SqliteRow>,
        now: i64,
    ) -> Result<Vec<PlayerDirectoryEntry>> {
        let groups = sqlx::query("SELECT affiliation_id,kind,name,parent_id,color_rgb FROM relationship_affiliations WHERE realm_id = ? AND dataset_id = ?").bind(realm).bind(dataset).fetch_all(&self.db).await?;
        let groups: HashMap<String, AffiliationGroup> = groups
            .into_iter()
            .map(|row| {
                let id = row.get::<String, _>("affiliation_id");
                (
                    id.clone(),
                    AffiliationGroup {
                        affiliation_id: id,
                        kind: row.get("kind"),
                        name: row.get("name"),
                        parent_affiliation_id: row.get("parent_id"),
                        color_rgb: row.get::<Option<i64>, _>("color_rgb").map(|v| v as u32),
                        attributes: BTreeMap::new().into_iter().collect(),
                    },
                )
            })
            .collect();
        let stale_after = sqlx::query("SELECT stale_after_seconds FROM relationship_datasets WHERE realm_id = ? AND dataset_id = ?").bind(realm).bind(dataset).fetch_optional(&self.db).await?.and_then(|row| row.get::<Option<i64>,_>("stale_after_seconds")).unwrap_or(DEFAULT_STALE_AFTER_MS/1000) * 1000;
        rows.into_iter()
            .map(|row| {
                let ids: Vec<String> =
                    serde_json::from_str(&row.get::<String, _>("affiliation_ids_json"))?;
                Ok(PlayerDirectoryEntry {
                    player: Some(PlayerIdentityRecord {
                        player_id: row.get("player_id"),
                        uuid: row.get("uuid"),
                        name: row.get("name"),
                        aliases: Vec::new(),
                        affiliation_ids: ids.clone(),
                        compatibility_tab_entry: None,
                    }),
                    affiliations: ids
                        .iter()
                        .filter_map(|id| groups.get(id).cloned())
                        .collect(),
                    dataset_id: dataset.to_owned(),
                    observed_at_utc_ms: row.get("observed_at"),
                    stale: now - row.get::<i64, _>("observed_at") > stale_after,
                })
            })
            .collect()
    }

    async fn relation_for(
        &self,
        realm: &str,
        dataset: Option<&str>,
        subject: &PlayerDirectoryEntry,
        target: &PlayerDirectoryEntry,
    ) -> Result<PlayerRelationResult> {
        let dataset = self.dataset_id(realm, dataset).await?;
        let subject_ids: BTreeSet<_> = subject
            .player
            .as_ref()
            .map(|p| p.affiliation_ids.iter().cloned().collect())
            .unwrap_or_default();
        let target_ids: BTreeSet<_> = target
            .player
            .as_ref()
            .map(|p| p.affiliation_ids.iter().cloned().collect())
            .unwrap_or_default();
        let mut evidence = Vec::new();
        if let Some(shared) = subject_ids.intersection(&target_ids).next() {
            evidence.push(PlayerRelationEvidence {
                relation: PlayerRelationKind::Friendly as i32,
                source_affiliation_id: shared.clone(),
                target_affiliation_id: shared.clone(),
                affiliation_relation: None,
                reason: "shared_affiliation".to_owned(),
            });
        }
        for from in &subject_ids {
            for to in &target_ids {
                for row in sqlx::query("SELECT relation FROM relationship_edges WHERE realm_id = ? AND dataset_id = ? AND ((from_id = ? AND to_id = ?) OR (from_id = ? AND to_id = ?))").bind(realm).bind(&dataset).bind(from).bind(to).bind(to).bind(from).fetch_all(&self.db).await? {
                let relation:i32 = row.get("relation"); evidence.push(PlayerRelationEvidence { relation: if relation == AffiliationRelationKind::Hostile as i32 { PlayerRelationKind::Hostile as i32 } else { PlayerRelationKind::Friendly as i32 }, source_affiliation_id:from.clone(), target_affiliation_id:to.clone(), affiliation_relation:Some(relation), reason:"explicit_affiliation_edge".to_owned() });
            }
            }
        }
        let has_friendly = evidence
            .iter()
            .any(|e| e.relation == PlayerRelationKind::Friendly as i32);
        let has_hostile = evidence
            .iter()
            .any(|e| e.relation == PlayerRelationKind::Hostile as i32);
        let relation = if has_friendly && has_hostile {
            PlayerRelationKind::Conflict
        } else if has_hostile {
            PlayerRelationKind::Hostile
        } else if has_friendly {
            PlayerRelationKind::Friendly
        } else {
            PlayerRelationKind::Neutral
        };
        Ok(PlayerRelationResult {
            target: Some(target.clone()),
            relation: relation as i32,
            evidence,
        })
    }

    async fn dataset_id(&self, realm: &str, dataset: Option<&str>) -> Result<String> {
        if let Some(dataset) = dataset {
            return Ok(dataset.to_owned());
        }
        Ok(sqlx::query("SELECT dataset_id FROM relationship_datasets WHERE realm_id = ? ORDER BY observed_at DESC LIMIT 1").bind(realm).fetch_optional(&self.db).await?.map(|row| row.get("dataset_id")).unwrap_or_else(|| "political-directory".to_owned()))
    }
}

fn normalize(value: &str) -> String {
    value.trim().nfkc().collect::<String>().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::teamviewer::v1::{
        AffiliationKind, DatasetCoverage, ExternalDatasetDescriptor, PlayerSelector,
        player_selector,
    };

    fn player(id: &str, name: &str, affiliations: &[&str]) -> PlayerIdentityRecord {
        PlayerIdentityRecord {
            player_id: id.to_owned(),
            uuid: Some(id.to_owned()),
            name: name.to_owned(),
            affiliation_ids: affiliations
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            ..Default::default()
        }
    }

    fn selector_name(name: &str) -> PlayerSelector {
        PlayerSelector {
            selector: Some(player_selector::Selector::Name(name.to_owned())),
        }
    }

    fn selector_id(id: &str) -> PlayerSelector {
        PlayerSelector {
            selector: Some(player_selector::Selector::PlayerId(id.to_owned())),
        }
    }

    #[tokio::test]
    async fn preserves_duplicate_names_and_resolves_conflicting_evidence() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let store = RelationshipStore::new(db);
        store.initialize().await.unwrap();
        let descriptor = ExternalDatasetDescriptor {
            realm_id: "realm".to_owned(),
            dataset_id: "politics".to_owned(),
            format: "fixture".to_owned(),
            coverage: DatasetCoverage::Complete as i32,
            stale_after_seconds: 10,
            ..Default::default()
        };
        let groups = [
            ("town:a", AffiliationKind::Town),
            ("town:b", AffiliationKind::Town),
            ("nation:n", AffiliationKind::Nation),
        ]
        .into_iter()
        .map(|(id, kind)| AffiliationGroup {
            affiliation_id: id.to_owned(),
            kind: kind as i32,
            name: id.to_owned(),
            ..Default::default()
        })
        .collect::<Vec<_>>();
        let players = vec![
            player(
                "00000000-0000-0000-0000-000000000001",
                "SameName",
                &["town:a", "nation:n"],
            ),
            player(
                "00000000-0000-0000-0000-000000000002",
                "SameName",
                &["town:b", "nation:n"],
            ),
            player("00000000-0000-0000-0000-000000000003", "Unaffiliated", &[]),
        ];
        let edge = AffiliationEdge {
            from_affiliation_id: "town:a".to_owned(),
            to_affiliation_id: "town:b".to_owned(),
            relation: AffiliationRelationKind::Hostile as i32,
        };
        store
            .upsert_chunk(
                &descriptor,
                &RelationshipGraphSnapshotChunk {
                    head: Some(RelationshipGraphHead {
                        revision: "1".to_owned(),
                        ..Default::default()
                    }),
                    players,
                    affiliations: groups,
                    edges: vec![edge],
                    ..Default::default()
                },
                1_000,
            )
            .await
            .unwrap();

        let matches = store
            .lookup("realm", None, &[selector_name(" samename ")], 20_000)
            .await
            .unwrap();
        assert_eq!(matches[0].len(), 2);
        assert!(matches[0].iter().all(|entry| entry.stale));

        let (_, relations) = store
            .relations(
                "realm",
                None,
                &selector_id("00000000-0000-0000-0000-000000000001"),
                &[selector_id("00000000-0000-0000-0000-000000000002")],
                1_000,
            )
            .await
            .unwrap();
        assert_eq!(relations[0].relation, PlayerRelationKind::Conflict as i32);
        assert_eq!(relations[0].evidence.len(), 2);

        let (_, neutral) = store
            .relations(
                "realm",
                None,
                &selector_id("00000000-0000-0000-0000-000000000001"),
                &[selector_id("00000000-0000-0000-0000-000000000003")],
                1_000,
            )
            .await
            .unwrap();
        assert_eq!(neutral[0].relation, PlayerRelationKind::Neutral as i32);
    }
}
