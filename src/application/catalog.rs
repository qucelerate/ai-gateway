use std::{io::Read, path::Path};
use thiserror::Error;

use crate::{
    domain::{catalog::CatalogManifest, identity::OrganizationId},
    persistence::catalog::{AppliedCatalogRelease, CatalogStore, CatalogStoreError},
};

#[derive(Debug, Error)]
pub enum CatalogApplicationError {
    #[error("could not read catalog manifest")]
    Read(#[from] std::io::Error),
    #[error("catalog manifest is invalid")]
    Manifest(#[from] crate::domain::catalog::CatalogError),
    #[error(transparent)]
    Store(#[from] CatalogStoreError),
}

#[derive(Clone)]
pub struct CatalogApplication {
    store: CatalogStore,
}

impl CatalogApplication {
    pub fn new(store: CatalogStore) -> Self {
        Self { store }
    }

    pub async fn apply_file(
        &self,
        organization_id: OrganizationId,
        file: &Path,
    ) -> Result<AppliedCatalogRelease, CatalogApplicationError> {
        let mut bytes = Vec::new();
        std::fs::File::open(file)?
            .take(crate::domain::catalog::MAX_MANIFEST_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > crate::domain::catalog::MAX_MANIFEST_BYTES {
            return Err(crate::domain::catalog::CatalogError::Bounds.into());
        }
        let (manifest, digest) = CatalogManifest::parse(&bytes)?;
        Ok(self.store.apply(organization_id, &manifest, digest).await?)
    }
}
