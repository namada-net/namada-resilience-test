use std::collections::{BTreeSet, HashMap};
use std::fmt::Display;

use cosmrs::Any;
use enum_dispatch::enum_dispatch;
use namada_sdk::{args, signing::SigningData, tx::Tx};
use tokio::time::{sleep, Duration};

use crate::check::Check;
use crate::constants::DEFAULT_GAS_LIMIT;
use crate::context::Ctx;
use crate::error::TaskError;
use crate::state::State;
use crate::types::{Alias, Fee, Height, MaspEpoch};
use crate::utils::{
    execute_cosmos_tx, execute_tx, get_block_height, get_masp_epoch, get_masp_epoch_at_height,
    retry_config, wait_block_settlement, wait_cosmos_settlement, RetryConfig,
};

pub mod batch;
pub mod become_validator;
pub mod bond;
pub mod change_consensus_key;
pub mod change_metadata;
pub mod claim_rewards;
pub mod deactivate_validator;
pub mod default_proposal;
pub mod faucet_transfer;
pub mod ibc_transfer;
pub mod init_account;
pub mod new_wallet_keypair;
pub mod reactivate_validator;
pub mod redelegate;
pub mod shielded;
pub mod shielding;
pub mod transparent_transfer;
pub mod unbond;
pub mod unshielding;
pub mod update_account;
pub mod vote;

#[derive(Clone, Debug)]
pub struct TaskSettings {
    pub signers: BTreeSet<Alias>,
    pub gas_payer: Alias,
    pub gas_limit: u64,
}

impl TaskSettings {
    pub fn new(signers: BTreeSet<Alias>, gas_payer: Alias) -> Self {
        Self {
            signers,
            gas_payer,
            gas_limit: DEFAULT_GAS_LIMIT,
        }
    }

    pub fn faucet() -> Self {
        Self {
            signers: BTreeSet::from_iter(vec![Alias::faucet()]),
            gas_payer: Alias::faucet(),
            gas_limit: DEFAULT_GAS_LIMIT,
        }
    }

    pub fn faucet_batch(size: usize) -> Self {
        Self {
            signers: BTreeSet::from_iter(vec![Alias::faucet()]),
            gas_payer: Alias::faucet(),
            gas_limit: DEFAULT_GAS_LIMIT * size as u64,
        }
    }
}

#[enum_dispatch]
#[derive(Clone, Debug)]
pub enum Task {
    NewWalletKeyPair(new_wallet_keypair::NewWalletKeyPair),
    FaucetTransfer(faucet_transfer::FaucetTransfer),
    TransparentTransfer(transparent_transfer::TransparentTransfer),
    IbcTransferSend(ibc_transfer::IbcTransferSend),
    IbcTransferRecv(ibc_transfer::IbcTransferRecv),
    IbcShieldingTransfer(ibc_transfer::IbcShieldingTransfer),
    IbcUnshieldingTransfer(ibc_transfer::IbcUnshieldingTransfer),
    Bond(bond::Bond),
    Unbond(unbond::Unbond),
    Redelegate(redelegate::Redelegate),
    ClaimRewards(claim_rewards::ClaimRewards),
    Batch(batch::Batch),
    ShieldedTransfer(shielded::ShieldedTransfer),
    Shielding(shielding::Shielding),
    InitAccount(init_account::InitAccount),
    Unshielding(unshielding::Unshielding),
    BecomeValidator(become_validator::BecomeValidator),
    ChangeMetadata(change_metadata::ChangeMetadata),
    ChangeConsensusKey(change_consensus_key::ChangeConsensusKey),
    DeactivateValidator(deactivate_validator::DeactivateValidator),
    ReactivateValidator(reactivate_validator::ReactivateValidator),
    UpdateAccount(update_account::UpdateAccount),
    DefaultProposal(default_proposal::DefaultProposal),
    Vote(vote::Vote),
}

impl Task {
    pub fn aggregate_fees(&self, fees: &mut HashMap<Alias, Fee>, is_successful: bool) {
        match self {
            Task::Batch(batch) => {
                let tasks = batch.tasks();
                if tasks.len() == 1 {
                    let task = tasks.first().expect("Task should exist");
                    if let Some(settings) = task.task_settings() {
                        *fees.entry(settings.gas_payer.clone()).or_insert(0) += settings.gas_limit;
                    }
                } else {
                    if is_successful {
                        tasks
                            .iter()
                            .filter(|task| {
                                matches!(task, Task::ShieldedTransfer(_) | Task::Unshielding(_))
                            })
                            .for_each(|task| {
                                let settings = task
                                    .task_settings()
                                    .expect("Shielded task should have settings");
                                let gas_payer = &settings.gas_payer;
                                if gas_payer.is_spending_key() {
                                    *fees.entry(gas_payer.clone()).or_insert(0) +=
                                        settings.gas_limit;
                                }
                            });
                    }
                    // fee for wrapper tx
                    let settings = batch.task_settings().expect("TaskSettings should exist");
                    *fees.entry(settings.gas_payer.clone()).or_insert(0) += settings.gas_limit;
                }
            }
            _ => {
                if let Some(settings) = self.task_settings() {
                    *fees.entry(settings.gas_payer.clone()).or_insert(0) += settings.gas_limit;
                }
            }
        }
    }
}

impl Display for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.summary())
    }
}

#[enum_dispatch(Task)]
pub trait TaskContext {
    fn name(&self) -> String;

    fn summary(&self) -> String;

    fn task_settings(&self) -> Option<&TaskSettings>;

    #[allow(async_fn_in_trait)]
    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError>;

    #[allow(async_fn_in_trait)]
    async fn execute(&self, ctx: &Ctx) -> Result<Height, TaskError> {
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

    #[allow(async_fn_in_trait)]
    async fn execute_shielded_tx(
        &self,
        ctx: &Ctx,
        start_epoch: MaspEpoch,
    ) -> Result<Height, TaskError> {
        let retry_config = retry_config();

        let height = get_block_height(ctx, retry_config).await?;
        let result = match self.build_tx(ctx).await {
            Ok((tx, signing_data, tx_args)) => execute_tx(ctx, tx, signing_data, &tx_args).await,
            Err(e) => Err(e),
        };

        let epoch = match result {
            Ok(height) => {
                wait_block_settlement(ctx, height, retry_config).await;
                None
            }
            Err(TaskError::Execution { height, .. })
            | Err(TaskError::InsufficientGas { height, .. }) => {
                wait_block_settlement(ctx, height, retry_config).await;
                Some(get_masp_epoch_at_height(ctx, height, retry_config).await?)
            }
            Err(_) => {
                wait_block_settlement(ctx, height + 1, retry_config).await;
                Some(get_masp_epoch(ctx, retry_config).await?)
            }
        };

        result.map_err(|err| {
            if epoch == Some(start_epoch) {
                err
            } else {
                TaskError::InvalidShielded {
                    err: err.to_string(),
                    was_fee_paid: matches!(err, TaskError::Execution { .. }),
                }
            }
        })
    }

    #[allow(async_fn_in_trait)]
    async fn build_cosmos_tx(&self, _ctx: &Ctx) -> Result<Any, TaskError> {
        unimplemented!("Implement for a tx on Cosmos")
    }

    #[allow(async_fn_in_trait)]
    async fn execute_cosmos_tx(&self, ctx: &Ctx) -> Result<Height, TaskError> {
        let any_msg = self.build_cosmos_tx(ctx).await?;
        let height = loop {
            match execute_cosmos_tx(ctx, any_msg.clone()).await {
                Err(TaskError::CosmosTx(ref e)) if e.contains("unauthorized") => {
                    tracing::warn!("retry for cosmos `unauthorized` error");
                    sleep(Duration::from_secs(1)).await;
                }
                Err(TaskError::Query(_)) => {
                    tracing::warn!("retry for cosmos query error");
                    sleep(Duration::from_secs(1)).await;
                }
                res => break res,
            }
        }?;
        wait_cosmos_settlement(ctx, height).await;
        Ok(height)
    }

    #[allow(async_fn_in_trait)]
    async fn build_checks(
        &self,
        ctx: &Ctx,
        retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError>;

    fn update_state(&self, state: &mut State);
}
