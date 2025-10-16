use std::str::FromStr;

use namada_sdk::address::Address;
use namada_sdk::args::{self, TxBuilder};
use namada_sdk::signing::SigningData;
use namada_sdk::token;
use namada_sdk::tx::Tx;
use namada_sdk::Namada;
use typed_builder::TypedBuilder;

use crate::check::{self, Check};
use crate::context::Ctx;
use crate::error::TaskError;
use crate::state::State;
use crate::task::{TaskContext, TaskSettings};
use crate::types::{Alias, Amount, Epoch, ValidatorAddress};
use crate::utils::{get_balance, get_bond, wrapper_tx, RetryConfig};

#[derive(Clone, Debug, TypedBuilder)]
pub struct Bond {
    source: Alias,
    validator: ValidatorAddress,
    amount: Amount,
    epoch: Epoch,
    settings: TaskSettings,
}

impl TaskContext for Bond {
    fn name(&self) -> String {
        "bond".to_string()
    }

    fn summary(&self) -> String {
        format!(
            "bond/{}/{}/{}",
            self.source.name, self.validator, self.amount
        )
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        let wallet = ctx.namada.wallet.read().await;

        let source_address = wallet
            .find_address(&self.source.name)
            .ok_or_else(|| TaskError::Wallet(format!("No source address: {}", self.source.name)))?;
        let token_amount = token::Amount::from_u64(self.amount);
        let validator =
            Address::from_str(&self.validator).expect("ValidatorAddress should be converted");

        let mut bond_tx_builder = ctx
            .namada
            .new_bond(validator, token_amount)
            .source(source_address.into_owned());
        let mut signing_keys = vec![];
        for signer in &self.settings.signers {
            let public_key = wallet
                .find_public_key(&signer.name)
                .map_err(|e| TaskError::Wallet(e.to_string()))?;
            signing_keys.push(public_key)
        }
        bond_tx_builder = bond_tx_builder.signing_keys(signing_keys);
        drop(wallet);

        let wrapper = wrapper_tx(
            ctx,
            Some(self.settings.gas_payer.clone()),
            Some(self.settings.gas_limit),
        )
        .await;
        bond_tx_builder = bond_tx_builder.wrap_it(Some(wrapper));

        let (bond_tx, signing_data) = bond_tx_builder
            .build(&ctx.namada)
            .await
            .map_err(|e| TaskError::BuildTx(e.to_string()))?;

        Ok((bond_tx, signing_data, bond_tx_builder.tx))
    }

    async fn build_checks(
        &self,
        ctx: &Ctx,
        retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        let pre_bond =
            get_bond(ctx, &self.source, &self.validator, self.epoch, retry_config).await?;

        let check_bond = Check::BondIncrease(
            check::bond_increase::BondIncrease::builder()
                .target(self.source.clone())
                .validator(self.validator.clone())
                .pre_bond(pre_bond)
                .epoch(self.epoch)
                .amount(self.amount)
                .build(),
        );

        let denom = Alias::nam().name;
        let (_, pre_balance) = get_balance(ctx, &self.source, &denom, retry_config).await?;
        let check_balance = Check::BalanceSource(
            check::balance_source::BalanceSource::builder()
                .target(self.source.clone())
                .pre_balance(pre_balance)
                .denom(denom)
                .amount(self.amount)
                .build(),
        );

        Ok(vec![check_bond, check_balance])
    }

    fn update_state(&self, state: &mut State) {
        state.modify_bond(&self.source, &self.validator, self.amount, self.epoch);
    }
}
