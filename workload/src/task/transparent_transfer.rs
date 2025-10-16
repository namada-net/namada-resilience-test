use namada_sdk::args::{self, TxBuilder, TxTransparentSource, TxTransparentTarget};
use namada_sdk::signing::SigningData;
use namada_sdk::tx::Tx;
use namada_sdk::Namada;
use typed_builder::TypedBuilder;

use crate::check::{self, Check};
use crate::context::Ctx;
use crate::error::TaskError;
use crate::state::State;
use crate::task::{TaskContext, TaskSettings};
use crate::types::{Alias, Amount};
use crate::utils::{get_balance, get_token, is_native_denom, wrapper_tx, RetryConfig};

#[derive(Clone, Debug, TypedBuilder)]
pub struct TransparentTransfer {
    source: Alias,
    target: Alias,
    denom: String,
    amount: Amount,
    settings: TaskSettings,
}

impl TaskContext for TransparentTransfer {
    fn name(&self) -> String {
        "transparent-transfer".to_string()
    }

    fn summary(&self) -> String {
        format!(
            "transparent-transfer/{}/{}/{}",
            self.source.name, self.target.name, self.amount
        )
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        let wallet = ctx.namada.wallet.read().await;

        let source_address = wallet
            .find_address(&self.source.name)
            .ok_or_else(|| TaskError::Wallet(format!("No source address: {}", self.source.name)))?
            .into_owned();
        let target_address = wallet
            .find_address(&self.target.name)
            .ok_or_else(|| TaskError::Wallet(format!("No target address: {}", self.target.name)))?
            .into_owned();
        let (token_address, amount) = get_token(ctx, &self.denom, self.amount).await?;

        let sources = vec![TxTransparentSource {
            source: source_address,
            token: token_address.clone(),
            amount,
        }];
        let targets = vec![TxTransparentTarget {
            target: target_address,
            token: token_address,
            amount,
        }];

        let mut transfer_tx_builder = ctx.namada.new_transparent_transfer(sources, targets);
        let mut signing_keys = vec![];
        for signer in &self.settings.signers {
            let public_key = wallet
                .find_public_key(&signer.name)
                .map_err(|e| TaskError::Wallet(e.to_string()))?;
            signing_keys.push(public_key)
        }
        transfer_tx_builder = transfer_tx_builder.signing_keys(signing_keys);
        drop(wallet);

        let wrapper = wrapper_tx(
            ctx,
            Some(self.settings.gas_payer.clone()),
            Some(self.settings.gas_limit),
        )
        .await;
        transfer_tx_builder = transfer_tx_builder.wrap_it(Some(wrapper));

        let (transfer_tx, signing_data) = transfer_tx_builder
            .build(&ctx.namada)
            .await
            .map_err(|e| TaskError::BuildTx(e.to_string()))?;

        Ok((transfer_tx, signing_data, transfer_tx_builder.tx))
    }

    async fn build_checks(
        &self,
        ctx: &Ctx,
        retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        let (_, pre_balance) = get_balance(ctx, &self.source, &self.denom, retry_config).await?;
        let source_check = Check::BalanceSource(
            check::balance_source::BalanceSource::builder()
                .target(self.source.clone())
                .pre_balance(pre_balance)
                .denom(self.denom.clone())
                .amount(self.amount)
                .build(),
        );

        let (_, pre_balance) = get_balance(ctx, &self.target, &self.denom, retry_config).await?;
        let target_check = Check::BalanceTarget(
            check::balance_target::BalanceTarget::builder()
                .target(self.target.clone())
                .pre_balance(pre_balance)
                .denom(self.denom.clone())
                .amount(self.amount)
                .build(),
        );

        Ok(vec![source_check, target_check])
    }

    fn update_state(&self, state: &mut State) {
        if is_native_denom(&self.denom) {
            state.decrease_balance(&self.source, self.amount);
            state.increase_balance(&self.target, self.amount);
        } else {
            state.decrease_ibc_balance(&self.source, &self.denom, self.amount);
            state.increase_ibc_balance(&self.target, &self.denom, self.amount);
        }
    }
}
