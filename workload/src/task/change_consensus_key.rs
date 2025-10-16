use namada_sdk::args::{self, TxBuilder};
use namada_sdk::key::{RefTo, SchemeType};
use namada_sdk::signing::SigningData;
use namada_sdk::tx::Tx;
use namada_sdk::Namada;
use rand::rngs::OsRng;
use typed_builder::TypedBuilder;

use crate::check::Check;
use crate::context::Ctx;
use crate::error::TaskError;
use crate::state::State;
use crate::task::{TaskContext, TaskSettings};
use crate::types::Alias;
use crate::utils::{wrapper_tx, RetryConfig};

#[derive(Clone, Debug, TypedBuilder)]
pub struct ChangeConsensusKey {
    source: Alias,
    consensus_alias: Alias,
    settings: TaskSettings,
}

impl TaskContext for ChangeConsensusKey {
    fn name(&self) -> String {
        "change-consensus-key".to_string()
    }

    fn summary(&self) -> String {
        format!("change-consensus-key/{}", self.source.name)
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        let mut wallet = ctx.namada.wallet.write().await;

        let consensus_pk = wallet
            .gen_store_secret_key(
                SchemeType::Ed25519,
                Some(self.consensus_alias.name.clone()),
                true,
                None,
                &mut OsRng,
            )
            .expect("Key generation should not fail.")
            .1
            .ref_to();

        let source_address = wallet
            .find_address(&self.source.name)
            .ok_or_else(|| TaskError::Wallet(format!("No source address: {}", self.source.name)))?;

        let mut change_consensus_key_builder = ctx
            .namada
            .new_change_consensus_key(source_address.into_owned(), consensus_pk);

        let mut signing_keys = vec![];
        for signer in &self.settings.signers {
            let public_key = wallet
                .find_public_key(&signer.name)
                .map_err(|e| TaskError::Wallet(e.to_string()))?;
            signing_keys.push(public_key)
        }
        change_consensus_key_builder = change_consensus_key_builder.signing_keys(signing_keys);
        drop(wallet);

        let wrapper = wrapper_tx(
            ctx,
            Some(self.settings.gas_payer.clone()),
            Some(self.settings.gas_limit),
        )
        .await;
        change_consensus_key_builder = change_consensus_key_builder.wrap_it(Some(wrapper));

        let (change_consensus_key, signing_data) = change_consensus_key_builder
            .build(&ctx.namada)
            .await
            .map_err(|e| TaskError::BuildTx(e.to_string()))?;

        Ok((
            change_consensus_key,
            signing_data,
            change_consensus_key_builder.tx,
        ))
    }

    async fn build_checks(
        &self,
        _ctx: &Ctx,
        _retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        Ok(vec![])
    }

    fn update_state(&self, _state: &mut State) {}
}
