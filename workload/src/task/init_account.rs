use std::collections::BTreeSet;

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
use crate::types::{Alias, Threshold};
use crate::utils::{wrapper_tx, RetryConfig};

#[derive(Clone, Debug, TypedBuilder)]
pub struct InitAccount {
    target: Alias,
    sources: BTreeSet<Alias>,
    threshold: Threshold,
    settings: TaskSettings,
}

impl TaskContext for InitAccount {
    fn name(&self) -> String {
        "init-account".to_string()
    }

    fn summary(&self) -> String {
        format!("init-account/{}/{}", self.target.name, self.threshold)
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        let wallet = ctx.namada.wallet.read().await;

        let mut public_keys = vec![];
        for source in &self.sources {
            let source_pk = wallet
                .find_public_key(&source.name)
                .map_err(|e| TaskError::Wallet(e.to_string()))?;
            public_keys.push(source_pk);
        }

        let mut init_account_builder = ctx
            .namada
            .new_init_account(public_keys, Some(self.threshold as u8))
            .initialized_account_alias(self.target.name.clone())
            .wallet_alias_force(true);

        let mut signing_keys = vec![];
        for signer in &self.settings.signers {
            let public_key = wallet
                .find_public_key(&signer.name)
                .map_err(|e| TaskError::Wallet(e.to_string()))?;
            signing_keys.push(public_key)
        }
        init_account_builder = init_account_builder.signing_keys(signing_keys);
        drop(wallet);

        let wrapper = wrapper_tx(
            ctx,
            Some(self.settings.gas_payer.clone()),
            Some(self.settings.gas_limit),
        )
        .await;
        init_account_builder = init_account_builder.wrap_it(Some(wrapper));

        let (init_account_tx, signing_data) = init_account_builder
            .build(&ctx.namada)
            .await
            .map_err(|e| TaskError::BuildTx(e.to_string()))?;

        Ok((init_account_tx, signing_data, init_account_builder.tx))
    }

    async fn build_checks(
        &self,
        _ctx: &Ctx,
        _retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        Ok(vec![Check::AccountExist(
            check::account_exist::AccountExist::builder()
                .target(self.target.clone())
                .threshold(self.threshold)
                .sources(self.sources.clone())
                .build(),
        )])
    }

    fn update_state(&self, state: &mut State) {
        state.add_established_account(&self.target, &self.sources, self.threshold);
    }
}
