use std::collections::BTreeMap;

use namada_sdk::args::{self, TxBuilder};
use namada_sdk::governance::cli::onchain::{DefaultProposal as Proposal, OnChainProposal};
use namada_sdk::signing::SigningData;
use namada_sdk::tx::Tx;
use namada_sdk::Namada;
use typed_builder::TypedBuilder;

use crate::check::{self, Check};
use crate::constants::PROPOSAL_DEPOSIT;
use crate::context::Ctx;
use crate::error::TaskError;
use crate::state::State;
use crate::task::{TaskContext, TaskSettings};
use crate::types::{Alias, Epoch};
use crate::utils::{get_balance, wrapper_tx, RetryConfig};

#[derive(Clone, Debug, TypedBuilder)]
pub struct DefaultProposal {
    source: Alias,
    start_epoch: Epoch,
    end_epoch: Epoch,
    grace_epoch: Epoch,
    settings: TaskSettings,
}

impl TaskContext for DefaultProposal {
    fn name(&self) -> String {
        "default-proposal".to_string()
    }

    fn summary(&self) -> String {
        format!("default-proposal/{}", self.source.name)
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        let wallet = ctx.namada.wallet.read().await;
        let source_address = wallet
            .find_address(&self.source.name)
            .ok_or_else(|| TaskError::Wallet(format!("No source address: {}", self.source.name)))?;

        let default_proposal = Proposal {
            proposal: OnChainProposal {
                content: BTreeMap::from_iter([("workload".to_string(), "tester".to_string())]),
                author: source_address.into_owned(),
                voting_start_epoch: self.start_epoch.into(),
                voting_end_epoch: self.end_epoch.into(),
                activation_epoch: self.grace_epoch.into(),
            },
            data: if self.start_epoch % 2 == 0 {
                Some(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
            } else {
                None
            },
        };
        let proposal_json =
            serde_json::to_string(&default_proposal).expect("Encoding proposal shouldn't fail");

        let mut default_proposal_tx_builder =
            ctx.namada.new_init_proposal(proposal_json.into_bytes());

        let mut signing_keys = vec![];
        for signer in &self.settings.signers {
            let public_key = wallet
                .find_public_key(&signer.name)
                .map_err(|e| TaskError::Wallet(e.to_string()))?;
            signing_keys.push(public_key)
        }
        default_proposal_tx_builder = default_proposal_tx_builder.signing_keys(signing_keys);
        drop(wallet);

        let wrapper = wrapper_tx(
            ctx,
            Some(self.settings.gas_payer.clone()),
            Some(self.settings.gas_limit),
        )
        .await;
        default_proposal_tx_builder = default_proposal_tx_builder.wrap_it(Some(wrapper));

        let (default_proposal, signing_data) = default_proposal_tx_builder
            .build(&ctx.namada)
            .await
            .map_err(|e| TaskError::BuildTx(e.to_string()))?;

        Ok((
            default_proposal,
            signing_data,
            default_proposal_tx_builder.tx,
        ))
    }

    async fn build_checks(
        &self,
        ctx: &Ctx,
        retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        let denom = Alias::nam().name;
        let (_, pre_balance) = get_balance(ctx, &self.source, &denom, retry_config).await?;

        Ok(vec![Check::BalanceSource(
            check::balance_source::BalanceSource::builder()
                .target(self.source.clone())
                .pre_balance(pre_balance)
                .denom(denom)
                .amount(PROPOSAL_DEPOSIT)
                .build(),
        )])
    }

    fn update_state(&self, state: &mut State) {
        state.decrease_balance(&self.source, PROPOSAL_DEPOSIT);
        // proposal will be added later
    }
}
