//! The signer's vault: the identity secret, in the worker's memory for the engine's life.
//!
//! The host passes the secret to every `start`; nothing here stores it. This puts the identity
//! key in the browser, next to the app, which Pubky's own direction for signers avoids: the
//! engine accepts it because the host already holds the key (it derives it from the wallet's
//! seed), and the signer needs it to enrol the identity and approve the engine's app.

use std::sync::{Mutex, MutexGuard, PoisonError};

use pubky_dispatch_signer::{AddOutcome, IdentitySecret, SecretVault, VaultError};

/// The identity secret, in memory.
pub struct MemoryVault(Mutex<Option<IdentitySecret>>);

impl MemoryVault {
    /// A vault holding `secret`.
    pub fn holding(secret: IdentitySecret) -> Self {
        Self(Mutex::new(Some(secret)))
    }

    fn held(&self) -> MutexGuard<'_, Option<IdentitySecret>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SecretVault for MemoryVault {
    fn load(&self) -> Result<Option<IdentitySecret>, VaultError> {
        Ok(self.held().clone())
    }

    fn add(&self, secret: &IdentitySecret) -> Result<AddOutcome, VaultError> {
        let mut held = self.held();
        if held.is_some() {
            return Ok(AddOutcome::Occupied);
        }
        *held = Some(secret.clone());
        Ok(AddOutcome::Stored)
    }

    /// Deletes the secret only when it is `expected`, under the one lock, as the vault's
    /// contract asks.
    fn delete(&self, expected: &IdentitySecret) -> Result<(), VaultError> {
        let mut held = self.held();
        if held
            .as_ref()
            .is_some_and(|secret| secret.as_bytes() == expected.as_bytes())
        {
            *held = None;
        }
        Ok(())
    }
}
