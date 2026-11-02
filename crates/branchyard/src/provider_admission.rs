//! Embedding policy for the resolved provider of every new turn.

use std::sync::Arc;

use crate::{projection, Error, Provider, TaskOptions, Yard};

/// A caller's admission policy for a turn's resolved provider and supplied options.
///
/// `actual` includes inheritance from a stored branch. `options.provider` is
/// what the caller supplied, so recovery with default options can be distinguished
/// from an explicitly approved provider. The callback is shared by yard clones,
/// is never serialized, and runs outside the hub lock. It must be pure: preparation
/// and final execution can both check the same turn.
pub type ProviderAdmission =
    Arc<dyn Fn(Option<&Provider>, &TaskOptions) -> Result<(), Error> + Send + Sync>;

impl Yard {
    /// Install an embedding policy for every provider admission on this yard and
    /// its clones. Replaces the previous policy. Without one, SDK and CLI callers
    /// remain unrestricted by this hook; normal provider validation still applies.
    ///
    /// A server can require that an inherited native provider equal the provider
    /// in its own approved task options, denying default recovery options before
    /// a waiting child is claimed or its worktree is materialized.
    pub fn use_provider_admission(&self, admission: ProviderAdmission) {
        *projection::lock(&self.hub.provider_admission) = Some(admission);
    }

    pub(crate) fn admit_provider(
        &self,
        actual: Option<&Provider>,
        options: &TaskOptions,
    ) -> Result<(), Error> {
        let admission = projection::lock(&self.hub.provider_admission).clone();
        match admission {
            Some(admission) => admission(actual, options),
            None => Ok(()),
        }
    }
}
