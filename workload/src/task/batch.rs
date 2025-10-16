use std::collections::HashMap;

use namada_sdk::{args, signing::SigningData, tx::Tx};
use typed_builder::TypedBuilder;

use crate::check::{self, Check};
use crate::constants::COSMOS_TOKEN;
use crate::context::Ctx;
use crate::error::TaskError;
use crate::state::State;
use crate::task::{Task, TaskContext, TaskSettings};
use crate::types::{Alias, Height};
use crate::utils::{
    execute_tx, get_balance, get_block_height, get_bond, get_shielded_balance, ibc_denom,
    is_native_denom, merge_tx, retry_config, wait_block_settlement, RetryConfig,
};

#[derive(Clone, Debug, TypedBuilder)]
pub struct Batch {
    tasks: Vec<Task>,
    settings: TaskSettings,
}

impl Batch {
    pub fn tasks(&self) -> Vec<&Task> {
        self.tasks.iter().collect()
    }
}

impl TaskContext for Batch {
    fn name(&self) -> String {
        "batch".to_string()
    }

    fn summary(&self) -> String {
        let tasks = self
            .tasks
            .iter()
            .map(|task| task.to_string())
            .collect::<Vec<String>>();
        format!("batch-{} -> {}", tasks.len(), tasks.join(" -> "))
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        let mut txs = vec![];
        for task in &self.tasks {
            let (tx, signing_data, _) = Box::pin(task.build_tx(ctx)).await?;
            txs.push((tx, signing_data));
        }

        merge_tx(ctx, txs, &self.settings).await
    }

    async fn execute(&self, ctx: &Ctx) -> Result<Height, TaskError> {
        if self.tasks.iter().any(|task| {
            matches!(
                task,
                Task::Shielding(_) | Task::ShieldedTransfer(_) | Task::Unshielding(_)
            )
        }) {
            let epoch = self
                .tasks
                .iter()
                .filter_map(|task| match task {
                    Task::Shielding(inner) => Some(inner.epoch()),
                    Task::ShieldedTransfer(inner) => Some(inner.epoch()),
                    Task::Unshielding(inner) => Some(inner.epoch()),
                    _ => None,
                })
                .min()
                .expect("Epoch should be set");
            self.execute_shielded_tx(ctx, epoch).await
        } else {
            let retry_config = retry_config();
            let (tx, signing_data, tx_args) = self.build_tx(ctx).await?;

            let start_height = get_block_height(ctx, retry_config)
                .await
                .unwrap_or_default();
            match execute_tx(ctx, tx, signing_data, &tx_args).await {
                Ok(height) => {
                    wait_block_settlement(ctx, height, retry_config).await;
                    Ok(height)
                }
                Err(e) => {
                    wait_block_settlement(ctx, start_height, retry_config).await;
                    Err(e)
                }
            }
        }
    }

    async fn build_checks(
        &self,
        ctx: &Ctx,
        retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        let mut checks = vec![];
        for task in &self.tasks {
            let task_checks = Box::pin(task.build_checks(ctx, retry_config)).await?;
            checks.extend(task_checks);
        }

        let mut prepared_checks = vec![];
        let mut balances: HashMap<Alias, i64> = HashMap::default();
        let mut shielded_balances: HashMap<Alias, i64> = HashMap::default();
        let mut ibc_balances: HashMap<Alias, i64> = HashMap::default();
        let mut bonds: HashMap<String, (u64, i64)> = HashMap::default();
        for check in checks {
            match check {
                Check::RevealPk(_) => prepared_checks.push(check),
                Check::BalanceSource(balance_source) => {
                    if is_native_denom(balance_source.denom()) {
                        balances
                            .entry(balance_source.target().clone())
                            .and_modify(|balance| *balance -= balance_source.amount() as i64)
                            .or_insert(-(balance_source.amount() as i64));
                    } else {
                        ibc_balances
                            .entry(balance_source.target().clone())
                            .and_modify(|balance| *balance -= balance_source.amount() as i64)
                            .or_insert(-(balance_source.amount() as i64));
                    }
                }
                Check::BalanceTarget(balance_target) => {
                    if is_native_denom(balance_target.denom()) {
                        balances
                            .entry(balance_target.target().clone())
                            .and_modify(|balance| *balance += balance_target.amount() as i64)
                            .or_insert(balance_target.amount() as i64);
                    } else {
                        ibc_balances
                            .entry(balance_target.target().clone())
                            .and_modify(|balance| *balance += balance_target.amount() as i64)
                            .or_insert(balance_target.amount() as i64);
                    }
                }
                Check::BalanceShieldedSource(balance_source) => {
                    if is_native_denom(balance_source.denom()) {
                        shielded_balances
                            .entry(balance_source.target().base().clone())
                            .and_modify(|balance| *balance -= balance_source.amount() as i64)
                            .or_insert(-(balance_source.amount() as i64));
                    } else {
                        ibc_balances
                            .entry(balance_source.target().spending_key().clone())
                            .and_modify(|balance| *balance -= balance_source.amount() as i64)
                            .or_insert(-(balance_source.amount() as i64));
                    }
                }
                Check::BalanceShieldedTarget(balance_target) => {
                    if is_native_denom(balance_target.denom()) {
                        shielded_balances
                            .entry(balance_target.target().base().clone())
                            .and_modify(|balance| *balance += balance_target.amount() as i64)
                            .or_insert(balance_target.amount() as i64);
                    } else {
                        ibc_balances
                            .entry(balance_target.target().payment_address().clone())
                            .and_modify(|balance| *balance += balance_target.amount() as i64)
                            .or_insert(balance_target.amount() as i64);
                    }
                }
                Check::BondIncrease(bond_increase) => {
                    bonds
                        .entry(format!(
                            "{}@{}",
                            bond_increase.target().name,
                            bond_increase.validator()
                        ))
                        .and_modify(|(_epoch, bond_amount)| {
                            *bond_amount += bond_increase.amount() as i64
                        })
                        .or_insert((bond_increase.epoch(), bond_increase.amount() as i64));
                }
                Check::BondDecrease(bond_decrease) => {
                    bonds
                        .entry(format!(
                            "{}@{}",
                            bond_decrease.target().name,
                            bond_decrease.validator()
                        ))
                        .and_modify(|(_epoch, bond_amount)| {
                            *bond_amount -= bond_decrease.amount() as i64
                        })
                        .or_insert((bond_decrease.epoch(), -(bond_decrease.amount() as i64)));
                }
                _ => {
                    return Err(TaskError::BuildCheck(format!(
                        "Unexpected check happened: {check}"
                    )))
                }
            }
        }

        let denom = Alias::nam().name;
        for (alias, amount) in balances {
            let (_, pre_balance) = get_balance(ctx, &alias, &denom, retry_config).await?;
            if amount >= 0 {
                prepared_checks.push(Check::BalanceTarget(
                    check::balance_target::BalanceTarget::builder()
                        .target(alias)
                        .pre_balance(pre_balance)
                        .denom(denom.clone())
                        .amount(amount.unsigned_abs())
                        .build(),
                ));
            } else {
                prepared_checks.push(Check::BalanceSource(
                    check::balance_source::BalanceSource::builder()
                        .target(alias)
                        .pre_balance(pre_balance)
                        .denom(denom.clone())
                        .amount(amount.unsigned_abs())
                        .build(),
                ));
            }
        }

        for (key, (epoch, amount)) in bonds {
            let (source, validator) = key.split_once('@').unwrap();
            let pre_bond =
                get_bond(ctx, &Alias::from(source), validator, epoch, retry_config).await?;
            if amount > 0 {
                prepared_checks.push(Check::BondIncrease(
                    check::bond_increase::BondIncrease::builder()
                        .target(Alias::from(source))
                        .validator(validator.to_owned())
                        .pre_bond(pre_bond)
                        .epoch(epoch)
                        .amount(amount.unsigned_abs())
                        .build(),
                ));
            } else {
                prepared_checks.push(Check::BondDecrease(
                    check::bond_decrease::BondDecrease::builder()
                        .target(Alias::from(source))
                        .validator(validator.to_owned())
                        .pre_bond(pre_bond)
                        .epoch(epoch)
                        .amount(amount.unsigned_abs())
                        .build(),
                ));
            }
        }

        for (alias, amount) in shielded_balances {
            // shielded-sync has been already done in each task.build_checks()
            let pre_balance = get_shielded_balance(ctx, &alias, &denom, retry_config)
                .await?
                .unwrap_or_default();
            if amount >= 0 {
                prepared_checks.push(Check::BalanceShieldedTarget(
                    check::balance_shielded_target::BalanceShieldedTarget::builder()
                        .target(alias.payment_address())
                        .pre_balance(pre_balance)
                        .denom(denom.clone())
                        .amount(amount.unsigned_abs())
                        .build(),
                ));
            } else {
                prepared_checks.push(Check::BalanceShieldedSource(
                    check::balance_shielded_source::BalanceShieldedSource::builder()
                        .target(alias.spending_key())
                        .pre_balance(pre_balance)
                        .denom(denom.clone())
                        .amount(amount.unsigned_abs())
                        .build(),
                ));
            }
        }

        let ibc_denom = ibc_denom(&ctx.namada_channel_id, COSMOS_TOKEN);
        for (alias, amount) in ibc_balances {
            if alias.is_spending_key() || alias.is_payment_address() {
                let pre_balance = get_shielded_balance(ctx, &alias, &ibc_denom, retry_config)
                    .await?
                    .unwrap_or_default();
                if amount >= 0 {
                    prepared_checks.push(Check::BalanceShieldedTarget(
                        check::balance_shielded_target::BalanceShieldedTarget::builder()
                            .target(alias.payment_address())
                            .pre_balance(pre_balance)
                            .denom(ibc_denom.clone())
                            .amount(amount.unsigned_abs())
                            .build(),
                    ));
                } else {
                    prepared_checks.push(Check::BalanceShieldedSource(
                        check::balance_shielded_source::BalanceShieldedSource::builder()
                            .target(alias.spending_key())
                            .pre_balance(pre_balance)
                            .denom(ibc_denom.clone())
                            .amount(amount.unsigned_abs())
                            .build(),
                    ));
                }
            } else {
                let (_, pre_balance) = get_balance(ctx, &alias, &ibc_denom, retry_config).await?;
                if amount >= 0 {
                    prepared_checks.push(Check::BalanceTarget(
                        check::balance_target::BalanceTarget::builder()
                            .target(alias)
                            .pre_balance(pre_balance)
                            .denom(ibc_denom.clone())
                            .amount(amount.unsigned_abs())
                            .build(),
                    ));
                } else {
                    prepared_checks.push(Check::BalanceSource(
                        check::balance_source::BalanceSource::builder()
                            .target(alias)
                            .pre_balance(pre_balance)
                            .denom(ibc_denom.clone())
                            .amount(amount.unsigned_abs())
                            .build(),
                    ));
                }
            }
        }

        Ok(prepared_checks)
    }

    fn update_state(&self, state: &mut State) {
        for task in &self.tasks {
            task.update_state(state);
        }
    }
}
