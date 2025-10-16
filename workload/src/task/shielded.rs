use namada_sdk::args::{self, TxBuilder, TxShieldedSource, TxShieldedTarget};
use namada_sdk::masp_primitives;
use namada_sdk::masp_primitives::transaction::components::sapling::builder::RngBuildParams;
use namada_sdk::masp_primitives::zip32::PseudoExtendedKey;
use namada_sdk::signing::SigningData;
use namada_sdk::tx::Tx;
use namada_sdk::Namada;
use rand::rngs::OsRng;
use typed_builder::TypedBuilder;

use crate::check::{self, Check};
use crate::context::Ctx;
use crate::error::TaskError;
use crate::state::State;
use crate::task::{TaskContext, TaskSettings};
use crate::types::{Alias, Amount, Height, MaspEpoch};
use crate::utils::{
    get_shielded_balance, get_token, is_native_denom, shielded_sync_with_retry, wrapper_tx,
    RetryConfig,
};

#[derive(Clone, Debug, TypedBuilder)]
pub struct ShieldedTransfer {
    source: Alias,
    target: Alias,
    denom: String,
    amount: Amount,
    epoch: MaspEpoch,
    settings: TaskSettings,
}

impl ShieldedTransfer {
    pub fn epoch(&self) -> MaspEpoch {
        self.epoch
    }
}

impl ShieldedTransfer {
    pub fn source(&self) -> &Alias {
        &self.source
    }
}

impl TaskContext for ShieldedTransfer {
    fn name(&self) -> String {
        "shielded-transfer".to_string()
    }

    fn summary(&self) -> String {
        format!(
            "shielded-transfer/{}/{}/{}",
            self.source.name, self.target.name, self.amount
        )
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        let mut bparams = RngBuildParams::new(OsRng);
        let mut wallet = ctx.namada.wallet.write().await;

        let source_spending_key = wallet
            .find_spending_key(&self.source.name, None)
            .map_err(|e| TaskError::Wallet(e.to_string()))?;
        let tmp = masp_primitives::zip32::ExtendedSpendingKey::from(source_spending_key);
        let pseudo_spending_key_from_spending_key = PseudoExtendedKey::from(tmp);
        let target_payment_address =
            *wallet.find_payment_addr(&self.target.name).ok_or_else(|| {
                TaskError::Wallet(format!("No payment address: {}", self.target.name))
            })?;

        let disposable_gas_payer = self.settings.gas_payer.is_spending_key();
        let gas_spending_key = if disposable_gas_payer {
            let spending_key = wallet
                .find_spending_key(&self.settings.gas_payer.name, None)
                .map_err(|e| TaskError::Wallet(e.to_string()))?;
            let tmp = masp_primitives::zip32::ExtendedSpendingKey::from(spending_key);
            Some(PseudoExtendedKey::from(tmp))
        } else {
            None
        };

        drop(wallet);

        let (token, amount) = get_token(ctx, &self.denom, self.amount).await?;

        let sources = vec![TxShieldedSource {
            source: pseudo_spending_key_from_spending_key,
            token: token.clone(),
            amount,
        }];
        let targets = vec![TxShieldedTarget {
            target: target_payment_address,
            token,
            amount,
        }];

        let mut transfer_tx_builder =
            ctx.namada
                .new_shielded_transfer(sources, targets, gas_spending_key);

        let wrapper = wrapper_tx(
            ctx,
            if disposable_gas_payer {
                None
            } else {
                Some(self.settings.gas_payer.clone())
            },
            Some(self.settings.gas_limit),
        )
        .await;
        transfer_tx_builder = transfer_tx_builder.wrap_it(Some(wrapper));

        // signing key isn't needed for shielded transfer

        let (transfer_tx, signing_data) = transfer_tx_builder
            .build(&ctx.namada, &mut bparams)
            .await
            .map_err(|e| TaskError::BuildTx(e.to_string()))?;

        Ok((transfer_tx, signing_data, transfer_tx_builder.tx))
    }

    async fn execute(&self, ctx: &Ctx) -> Result<Height, TaskError> {
        self.execute_shielded_tx(ctx, self.epoch).await
    }

    async fn build_checks(
        &self,
        ctx: &Ctx,
        retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        shielded_sync_with_retry(ctx, &self.source, None, false, retry_config).await?;

        let pre_balance = get_shielded_balance(ctx, &self.source, &self.denom, retry_config)
            .await?
            .unwrap_or_default();
        let source_check = Check::BalanceShieldedSource(
            check::balance_shielded_source::BalanceShieldedSource::builder()
                .target(self.source.clone())
                .pre_balance(pre_balance)
                .denom(self.denom.clone())
                .amount(self.amount)
                .build(),
        );

        let pre_balance = get_shielded_balance(ctx, &self.target, &self.denom, retry_config)
            .await?
            .unwrap_or_default();
        let target_check = Check::BalanceShieldedTarget(
            check::balance_shielded_target::BalanceShieldedTarget::builder()
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
            state.decrease_masp_balance(&self.source, self.amount);
            state.increase_masp_balance(&self.target, self.amount);
        } else {
            state.decrease_ibc_balance(&self.source, &self.denom, self.amount);
            state.increase_ibc_balance(&self.target, &self.denom, self.amount);
        }
    }
}
