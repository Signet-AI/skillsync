use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum BranchPolicy {
    RemoteDefault,
    Explicit { name: String },
}

impl BranchPolicy {
    pub(crate) fn explicit(name: &str) -> Result<Self> {
        crate::repository::validate_branch(name)?;
        Ok(Self::Explicit { name: name.into() })
    }

    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::RemoteDefault => Ok(()),
            Self::Explicit { name } => crate::repository::validate_branch(name),
        }
    }

    pub(crate) fn validate_effective_branch(&self, branch: &str) -> Result<()> {
        self.validate()?;
        if let Self::Explicit { name } = self {
            if name != branch {
                return Err(anyhow!(
                    "explicit branch policy does not match subscription branch"
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn requested_branch(&self) -> Option<&str> {
        match self {
            Self::RemoteDefault => None,
            Self::Explicit { name } => Some(name),
        }
    }

    pub(crate) fn migrate_legacy(branch: &str) -> Result<Self> {
        Self::explicit(branch).map_err(|e| anyhow!("invalid legacy subscription branch: {e}"))
    }
}
