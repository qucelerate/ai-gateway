//! An organization's master records and links (Phase 13, D-033). Catalog
//! metadata only: nothing here is read by routing, pricing or configuration
//! versions. Deletes are soft, and every state is kept as a revision.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    identity::OrganizationId,
    model_card::GovernanceActor,
    model_master::{MasterLink, Modality, ORG_PREFIX, OrgMaster},
};

#[derive(Clone)]
pub struct ModelMasterStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct RecordRow {
    slug: String,
    name: String,
    modality: String,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    updated_by: Option<Uuid>,
    updated_by_name: Option<String>,
    deleted: bool,
}

impl RecordRow {
    fn into_record(self) -> OrgMaster {
        OrgMaster {
            id: format!("{ORG_PREFIX}{}", self.slug),
            name: self.name,
            modality: Modality::parse(&self.modality).unwrap_or_default(),
            version: version(self.version),
            created_at: self.created_at,
            updated_at: self.updated_at,
            updated_by: actor(self.updated_by, self.updated_by_name),
            deleted: self.deleted,
        }
    }
}

#[derive(FromRow)]
struct LinkRow {
    family: String,
    model: String,
    master: Option<String>,
    version: Option<i64>,
    updated_at: DateTime<Utc>,
    updated_by: Option<Uuid>,
    updated_by_name: Option<String>,
    cleared: bool,
}

impl LinkRow {
    fn into_link(self) -> MasterLink {
        MasterLink {
            provider_family: self.family,
            model: self.model,
            master: self.master,
            version: version(self.version),
            updated_at: self.updated_at,
            updated_by: actor(self.updated_by, self.updated_by_name),
            cleared: self.cleared,
        }
    }
}

fn version(value: Option<i64>) -> u64 {
    value.and_then(|v| u64::try_from(v).ok()).unwrap_or(1)
}

fn actor(id: Option<Uuid>, name: Option<String>) -> Option<GovernanceActor> {
    id.map(|id| GovernanceActor {
        actor_type: "user".into(),
        id: Some(id),
        name,
    })
}

const RECORD_SELECT: &str = "SELECT m.slug, m.name, m.modality, v.version, m.created_at,
        m.updated_at, m.updated_by, u.display_name AS updated_by_name,
        m.deleted_at IS NOT NULL AS deleted
    FROM gateway.model_master m
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = m.organization_id
     AND v.resource_type = 'model_master' AND v.resource_id = m.id
    LEFT JOIN gateway.user_account u ON u.id = m.updated_by
    WHERE m.organization_id = $1";

const LINK_SELECT: &str = "SELECT l.family, l.model,
        CASE WHEN l.deleted_at IS NULL THEN l.master END AS master, v.version,
        CASE WHEN l.deleted_at IS NULL THEN l.updated_at ELSE l.deleted_at END AS updated_at,
        CASE WHEN l.deleted_at IS NULL THEN l.updated_by ELSE l.deleted_by END AS updated_by,
        u.display_name AS updated_by_name, l.deleted_at IS NOT NULL AS cleared
    FROM gateway.model_master_link l
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = l.organization_id
     AND v.resource_type = 'model_master_link' AND v.resource_id = l.id
    LEFT JOIN gateway.user_account u
      ON u.id = CASE WHEN l.deleted_at IS NULL THEN l.updated_by ELSE l.deleted_by END
    WHERE l.organization_id = $1";

/// Every (family, model) a card's `matches` lists, with its card: what the
/// API keeps in `model_card_match` for the database to stamp usage with.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CardMatches {
    families: Vec<String>,
    models: Vec<String>,
    cards: Vec<String>,
}

impl CardMatches {
    pub fn of(library: &crate::domain::model_card_files::CardLibrary) -> Self {
        let mut matches = Self::default();
        for (card, _) in library.cards() {
            for (family, listed) in &card.matches {
                for model in listed {
                    matches.families.push(family.clone());
                    matches.models.push(model.clone());
                    matches.cards.push(card.id.clone());
                }
            }
        }
        matches
    }

    pub fn len(&self) -> usize {
        self.families.len()
    }

    pub fn is_empty(&self) -> bool {
        self.families.is_empty()
    }
}

impl ModelMasterStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Replaces the card files' exact matches the database stamps usage
    /// with (migration 0141) by `library`'s: see [`CardMatches`]. Returns
    /// how many matches are stored.
    pub async fn sync_card_matches(
        &self,
        library: &crate::domain::model_card_files::CardLibrary,
    ) -> Result<usize, sqlx::Error> {
        self.store_card_matches(&CardMatches::of(library)).await
    }

    /// Replaces the stored card matches by `matches`.
    pub async fn store_card_matches(&self, matches: &CardMatches) -> Result<usize, sqlx::Error> {
        let CardMatches {
            families,
            models,
            cards,
        } = matches;
        let mut tx = self.pool.begin().await?;
        // Replicas starting together write one after the other.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('gateway.model_card_match'))")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM gateway.model_card_match m
             WHERE NOT EXISTS (
               SELECT 1 FROM unnest($1::text[], $2::text[], $3::text[]) AS w(family, model, card_id)
               WHERE w.family = m.family AND w.model = m.model AND w.card_id = m.card_id)",
        )
        .bind(families)
        .bind(models)
        .bind(cards)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO gateway.model_card_match (family, model, card_id)
             SELECT * FROM unnest($1::text[], $2::text[], $3::text[])
             ON CONFLICT (family, model) DO UPDATE SET card_id = EXCLUDED.card_id
             WHERE gateway.model_card_match.card_id IS DISTINCT FROM EXCLUDED.card_id",
        )
        .bind(families)
        .bind(models)
        .bind(cards)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(families.len())
    }

    /// The organization's records (not deleted), by slug.
    pub async fn records(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Vec<OrgMaster>, sqlx::Error> {
        let rows = sqlx::query_as::<_, RecordRow>(&format!(
            "{RECORD_SELECT} AND m.deleted_at IS NULL ORDER BY m.slug"
        ))
        .bind(organization_id.0)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(RecordRow::into_record).collect())
    }

    /// The organization's deleted records, by slug, each with the version
    /// it is created again at.
    pub async fn deleted_records(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Vec<OrgMaster>, sqlx::Error> {
        let rows = sqlx::query_as::<_, RecordRow>(&format!(
            "{RECORD_SELECT} AND m.deleted_at IS NOT NULL ORDER BY m.slug"
        ))
        .bind(organization_id.0)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(RecordRow::into_record).collect())
    }

    /// The record `slug`, unless deleted.
    pub async fn record(
        &self,
        organization_id: OrganizationId,
        slug: &str,
    ) -> Result<Option<OrgMaster>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::record_in(&mut conn, organization_id, slug).await
    }

    pub async fn record_in(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        slug: &str,
    ) -> Result<Option<OrgMaster>, sqlx::Error> {
        let row = sqlx::query_as::<_, RecordRow>(&format!(
            "{RECORD_SELECT} AND m.slug = $2 AND m.deleted_at IS NULL"
        ))
        .bind(organization_id.0)
        .bind(slug)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(RecordRow::into_record))
    }

    /// Creates the record, replaces its name and modality, or brings back a
    /// deleted one, and keeps the state as a revision. Returns true when no
    /// record was in use before (created or brought back).
    #[allow(clippy::too_many_arguments)]
    pub async fn put_record(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        slug: &str,
        name: &str,
        modality: Modality,
        actor: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let was_live: Option<bool> = sqlx::query_scalar(
            "SELECT deleted_at IS NULL FROM gateway.model_master
             WHERE organization_id = $1 AND slug = $2 FOR UPDATE",
        )
        .bind(organization_id.0)
        .bind(slug)
        .fetch_optional(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO gateway.model_master
                 (organization_id, id, slug, name, modality, created_by, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $6)
             ON CONFLICT (organization_id, slug) DO UPDATE
             SET name = EXCLUDED.name, modality = EXCLUDED.modality,
                 updated_by = EXCLUDED.updated_by, updated_at = CURRENT_TIMESTAMP,
                 deleted_at = NULL, deleted_by = NULL",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(slug)
        .bind(name)
        .bind(modality.as_str())
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        Self::record_revision(conn, organization_id, id, name, modality, false, actor).await?;
        Ok(was_live != Some(true))
    }

    /// Deletes the record (soft). Returns false when none is in use.
    pub async fn delete_record(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        slug: &str,
        actor: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let row: Option<(Uuid, String, String)> = sqlx::query_as(
            "UPDATE gateway.model_master
             SET deleted_at = CURRENT_TIMESTAMP, deleted_by = $3
             WHERE organization_id = $1 AND slug = $2 AND deleted_at IS NULL
             RETURNING id, name, modality",
        )
        .bind(organization_id.0)
        .bind(slug)
        .bind(actor)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((id, name, modality)) = row else {
            return Ok(false);
        };
        let modality = Modality::parse(&modality).unwrap_or_default();
        Self::record_revision(conn, organization_id, id, &name, modality, true, actor).await?;
        Ok(true)
    }

    async fn record_revision(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        name: &str,
        modality: Modality,
        deleted: bool,
        actor: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO gateway.model_master_revision
                 (id, organization_id, master_id, name, modality, deleted, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::new_v4())
        .bind(organization_id.0)
        .bind(id)
        .bind(name)
        .bind(modality.as_str())
        .bind(deleted)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// How many links (in use) name `master`, locked so none is set to it
    /// while it is deleted.
    pub async fn links_to(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        master: &str,
    ) -> Result<i64, sqlx::Error> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM gateway.model_master_link
             WHERE organization_id = $1 AND master = $2 AND deleted_at IS NULL
             FOR UPDATE",
        )
        .bind(organization_id.0)
        .bind(master)
        .fetch_all(&mut *conn)
        .await?;
        Ok(i64::try_from(ids.len()).unwrap_or(i64::MAX))
    }

    /// Whether the record `slug` is in use, locked so it is not deleted
    /// while a link to it is set.
    pub async fn record_live_for_link(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        slug: &str,
    ) -> Result<Option<Modality>, sqlx::Error> {
        let modality: Option<String> = sqlx::query_scalar(
            "SELECT modality FROM gateway.model_master
             WHERE organization_id = $1 AND slug = $2 AND deleted_at IS NULL
             FOR SHARE",
        )
        .bind(organization_id.0)
        .bind(slug)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(modality.and_then(|m| Modality::parse(&m)))
    }

    /// The operations the active release serves for `model` on `family`,
    /// read in the caller's transaction; empty when it serves none (or no
    /// release is active).
    pub async fn served_operations_in(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        family: &str,
        model: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT DISTINCT c.operation
             FROM gateway.catalog_active_release a
             JOIN gateway.catalog_deployment d
               ON d.organization_id = a.organization_id AND d.release_id = a.release_id
             JOIN gateway.catalog_provider_account p
               ON p.organization_id = d.organization_id AND p.release_id = d.release_id
              AND p.id = d.provider_account_id
             JOIN gateway.catalog_capability c
               ON c.organization_id = d.organization_id AND c.release_id = d.release_id
              AND c.deployment_id = d.id
             WHERE a.organization_id = $1 AND p.family = $2 AND d.model = $3
             ORDER BY c.operation",
        )
        .bind(organization_id.0)
        .bind(family)
        .bind(model)
        .fetch_all(&mut *conn)
        .await
    }

    /// Every link in use, by family and model.
    pub async fn links(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Vec<MasterLink>, sqlx::Error> {
        let mut links = self.every_link(organization_id).await?;
        links.retain(|link| !link.cleared);
        Ok(links)
    }

    /// Every link ever set, cleared ones included, by family and model.
    pub async fn every_link(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Vec<MasterLink>, sqlx::Error> {
        let rows =
            sqlx::query_as::<_, LinkRow>(&format!("{LINK_SELECT} ORDER BY l.family, l.model"))
                .bind(organization_id.0)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(LinkRow::into_link).collect())
    }

    /// The link for `family` and `model`, if one was ever set (cleared
    /// included: its version is the one to set it again at).
    pub async fn link(
        &self,
        organization_id: OrganizationId,
        family: &str,
        model: &str,
    ) -> Result<Option<MasterLink>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::link_in(&mut conn, organization_id, family, model).await
    }

    pub async fn link_in(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        family: &str,
        model: &str,
    ) -> Result<Option<MasterLink>, sqlx::Error> {
        let row = sqlx::query_as::<_, LinkRow>(&format!(
            "{LINK_SELECT} AND l.family = $2 AND l.model = $3"
        ))
        .bind(organization_id.0)
        .bind(family)
        .bind(model)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(LinkRow::into_link))
    }

    /// Sets the link (`master` None: to none), bringing back a cleared one,
    /// and keeps it as a revision. Returns true when no link was in use
    /// before.
    pub async fn set_link(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        family: &str,
        model: &str,
        master: Option<&str>,
        actor: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let was_live: Option<bool> = sqlx::query_scalar(
            "SELECT deleted_at IS NULL FROM gateway.model_master_link
             WHERE organization_id = $1 AND family = $2 AND model = $3 FOR UPDATE",
        )
        .bind(organization_id.0)
        .bind(family)
        .bind(model)
        .fetch_optional(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO gateway.model_master_link
                 (organization_id, id, family, model, master, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (organization_id, family, model) DO UPDATE
             SET master = EXCLUDED.master, updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP, deleted_at = NULL, deleted_by = NULL",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(family)
        .bind(model)
        .bind(master)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        Self::link_revision(conn, organization_id, id, master, false, actor).await?;
        Ok(was_live != Some(true))
    }

    /// Clears the link (soft), so cards decide again. Returns false when
    /// none is in use.
    pub async fn clear_link(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        family: &str,
        model: &str,
        actor: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "UPDATE gateway.model_master_link
             SET deleted_at = CURRENT_TIMESTAMP, deleted_by = $4
             WHERE organization_id = $1 AND family = $2 AND model = $3 AND deleted_at IS NULL
             RETURNING id",
        )
        .bind(organization_id.0)
        .bind(family)
        .bind(model)
        .bind(actor)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(id) = id else {
            return Ok(false);
        };
        Self::link_revision(conn, organization_id, id, None, true, actor).await?;
        Ok(true)
    }

    async fn link_revision(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        master: Option<&str>,
        cleared: bool,
        actor: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO gateway.model_master_link_revision
                 (id, organization_id, link_id, master, cleared, created_by)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::new_v4())
        .bind(organization_id.0)
        .bind(id)
        .bind(master)
        .bind(cleared)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }
}
