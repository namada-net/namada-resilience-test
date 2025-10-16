use namada_sdk::args::{self, TxBuilder};
use namada_sdk::signing::SigningData;
use namada_sdk::tx::Tx;
use namada_sdk::Namada;
use typed_builder::TypedBuilder;

use crate::check::{self, Check};
use crate::context::Ctx;
use crate::error::TaskError;
use crate::state::State;
use crate::task::{TaskContext, TaskSettings};
use crate::types::{Alias, ValidatorStatus};
use crate::utils::{wrapper_tx, RetryConfig};

#[derive(Clone, Debug, TypedBuilder)]
pub struct ReactivateValidator {
    target: Alias,
    settings: TaskSettings,
}

impl TaskContext for ReactivateValidator {
    fn name(&self) -> String {
        "reactivate-validator".to_string()
    }

    fn summary(&self) -> String {
        format!("reactivate-validator/{}", self.target.name)
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        let wallet = ctx.namada.wallet.read().await;
        let target_address = wallet
            .find_address(&self.target.name)
            .ok_or_else(|| TaskError::Wallet(format!("No target address: {}", self.target.name)))?;

        let mut reactivate_validator_builder_tx = ctx
            .namada
            .new_reactivate_validator(target_address.into_owned());

        let mut signing_keys = vec![];
        for signer in &self.settings.signers {
            let public_key = wallet
                .find_public_key(&signer.name)
                .map_err(|e| TaskError::Wallet(e.to_string()))?;
            signing_keys.push(public_key)
        }
        reactivate_validator_builder_tx =
            reactivate_validator_builder_tx.signing_keys(signing_keys);

        let wrapper = wrapper_tx(
            ctx,
            Some(self.settings.gas_payer.clone()),
            Some(self.settings.gas_limit),
        )
        .await;
        reactivate_validator_builder_tx = reactivate_validator_builder_tx.wrap_it(Some(wrapper));

        let (reactivate_validator, signing_data) = reactivate_validator_builder_tx
            .build(&ctx.namada)
            .await
            .map_err(|e| TaskError::BuildTx(e.to_string()))?;

        Ok((
            reactivate_validator,
            signing_data,
            reactivate_validator_builder_tx.tx,
        ))
    }

    async fn build_checks(
        &self,
        _ctx: &Ctx,
        _retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        Ok(vec![Check::ValidatorStatus(
            check::validator_status::ValidatorStatus::builder()
                .target(self.target.clone())
                .status(ValidatorStatus::Reactivating)
                .build(),
        )])
    }

    fn update_state(&self, state: &mut State) {
        state.reactivate_validator(&self.target);
    }
}
