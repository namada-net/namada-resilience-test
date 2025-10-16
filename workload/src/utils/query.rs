use std::collections::HashMap;
use std::str::FromStr;
use std::time::{self, Instant};

use namada_sdk::account::Account;
use namada_sdk::address::Address;
use namada_sdk::args::InputAmount;
use namada_sdk::control_flow::install_shutdown_signal;
use namada_sdk::io::DevNullProgressBar;
use namada_sdk::masp::shielded_wallet::ShieldedApi;
use namada_sdk::masp::{IndexerMaspClient, LedgerMaspClient, MaspLocalTaskEnv, ShieldedSyncConfig};
use namada_sdk::masp_primitives::zip32;
use namada_sdk::proof_of_stake::types::ValidatorStateInfo;
use namada_sdk::token::{self, DenominatedAmount, MaspEpoch};
use namada_sdk::{rpc, Namada};
use namada_wallet::DatedKeypair;
use reqwest::Url;
use tokio::time::{sleep, Duration};
use tryhard::{backoff_strategies::ExponentialBackoff, NoOnRetry, RetryFutureConfig};

use crate::context::Ctx;
use crate::error::QueryError;
use crate::types::{Alias, Amount, Epoch, Height, ProposalId, ProposalVote};
use crate::utils::{ibc_token_address, is_native_denom, RetryConfig};

pub async fn get_token(
    ctx: &Ctx,
    denom: &str,
    amount: Amount,
) -> Result<(Address, InputAmount), QueryError> {
    let wallet = ctx.namada.wallet.read().await;

    let token_amount = token::Amount::from_u64(amount);
    let (token_address, denominated_amount) = if is_native_denom(denom) {
        let address = wallet
            .find_address(denom)
            .ok_or_else(|| QueryError::Wallet(format!("No native token address: {}", denom)))?
            .into_owned();
        (address, DenominatedAmount::native(token_amount))
    } else {
        (
            ibc_token_address(denom),
            DenominatedAmount::new(token_amount, 0u8.into()),
        )
    };
    let input_amount = InputAmount::Unvalidated(denominated_amount);

    Ok((token_address, input_amount))
}

pub async fn get_account_info(
    ctx: &Ctx,
    source: &Alias,
    retry_config: RetryConfig,
) -> Result<(Address, Option<Account>), QueryError> {
    let wallet = ctx.namada.wallet.read().await;
    let source_address = wallet
        .find_address(&source.name)
        .ok_or_else(|| QueryError::Wallet(format!("No account address: {}", source.name)))?
        .into_owned();
    drop(wallet);

    let account = tryhard::retry_fn(|| rpc::get_account_info(&ctx.namada.client, &source_address))
        .with_config(retry_config)
        .on_retry(|attempt, _, error| {
            let error = error.to_string();
            async move {
                tracing::info!("Retry {} due to {}...", attempt, error);
            }
        })
        .await
        .map_err(QueryError::Rpc)?;

    Ok((source_address, account))
}

pub async fn is_validator(
    ctx: &Ctx,
    target: &Alias,
    retry_config: RetryConfig,
) -> Result<(Address, bool), QueryError> {
    let wallet = ctx.namada.wallet.read().await;
    let source_address = wallet
        .find_address(&target.name)
        .ok_or_else(|| QueryError::Wallet(format!("No target address: {}", target.name)))?
        .into_owned();
    drop(wallet);

    let is_validator = tryhard::retry_fn(|| rpc::is_validator(&ctx.namada.client, &source_address))
        .with_config(retry_config)
        .on_retry(|attempt, _, error| {
            let error = error.to_string();
            async move {
                tracing::info!("Retry {} due to {}...", attempt, error);
            }
        })
        .await
        .map_err(QueryError::Rpc)?;

    Ok((source_address, is_validator))
}

pub async fn get_validator_state(
    ctx: &Ctx,
    target: &Alias,
    epoch: Epoch,
    retry_config: RetryConfig,
) -> Result<(Address, ValidatorStateInfo), QueryError> {
    let wallet = ctx.namada.wallet.read().await;
    let target_address = wallet
        .find_address(&target.name)
        .ok_or_else(|| QueryError::Wallet(format!("No target address: {}", target.name)))?
        .into_owned();
    drop(wallet);

    let state = tryhard::retry_fn(|| {
        rpc::get_validator_state(&ctx.namada.client, &target_address, Some(epoch.into()))
    })
    .with_config(retry_config)
    .on_retry(|attempt, _, error| {
        let error = error.to_string();
        async move {
            tracing::info!("Retry {attempt} due to {error}...");
        }
    })
    .await
    .map_err(QueryError::Rpc)?;

    Ok((target_address, state))
}

pub async fn get_validator_addresses(
    ctx: &Ctx,
    retry_config: RetryConfig,
) -> Result<Vec<Address>, QueryError> {
    let current_epoch = get_epoch(ctx, retry_config).await?;
    let validators = rpc::get_all_consensus_validators(&ctx.namada.client, current_epoch.into())
        .await
        .map_err(QueryError::Rpc)?
        .into_iter()
        .map(|v| v.address)
        .collect();

    Ok(validators)
}

pub async fn is_pk_revealed(
    ctx: &Ctx,
    target: &Alias,
    retry_config: RetryConfig,
) -> Result<bool, QueryError> {
    let wallet = ctx.namada.wallet.read().await;
    let target_address = wallet
        .find_address(&target.name)
        .ok_or_else(|| QueryError::Wallet(format!("No target address: {}", target.name)))?
        .into_owned();
    drop(wallet);

    tryhard::retry_fn(|| rpc::is_public_key_revealed(&ctx.namada.client, &target_address))
        .with_config(retry_config)
        .on_retry(|attempt, _, error| {
            let error = error.to_string();
            async move {
                tracing::info!("Retry {attempt} due to {error}...");
            }
        })
        .await
        .map_err(QueryError::Rpc)
}

pub async fn get_balance(
    ctx: &Ctx,
    source: &Alias,
    denom: &str,
    retry_config: RetryConfig,
) -> Result<(Address, token::Amount), QueryError> {
    let wallet = ctx.namada.wallet.read().await;
    let token_address = if is_native_denom(denom) {
        wallet
            .find_address(denom)
            .ok_or_else(|| QueryError::Wallet(format!("No native token address: {denom}",)))?
            .into_owned()
    } else {
        ibc_token_address(denom)
    };
    let target_address = wallet
        .find_address(&source.name)
        .ok_or_else(|| QueryError::Wallet(format!("No target address: {}", source.name)))?;

    let balance = tryhard::retry_fn(|| {
        rpc::get_token_balance(&ctx.namada.client, &token_address, &target_address, None)
    })
    .with_config(retry_config)
    .on_retry(|attempt, _, error| {
        let error = error.to_string();
        async move {
            tracing::info!("Retry {attempt} due to {error}...");
        }
    })
    .await
    .map_err(QueryError::Rpc)?;

    Ok((target_address.into_owned(), balance))
}

/// Shielded balance. Need shielded-sync in advance.
pub async fn get_shielded_balance(
    ctx: &Ctx,
    source: &Alias,
    denom: &str,
    retry_config: RetryConfig,
) -> Result<Option<token::Amount>, QueryError> {
    let client = &ctx.namada.client;

    let masp_epoch = get_masp_epoch(ctx, retry_config).await?;

    let mut wallet = ctx.namada.wallet.write().await;
    let token_address = if is_native_denom(denom) {
        wallet
            .find_address(denom)
            .ok_or_else(|| QueryError::Wallet(format!("No native token address: {denom}",)))?
            .into_owned()
    } else {
        ibc_token_address(denom)
    };
    let spending_key = source.spending_key().name;
    let target_spending_key = wallet
        .find_spending_key(&spending_key, None)
        .map_err(|e| QueryError::Wallet(e.to_string()))?
        .to_owned();
    drop(wallet);

    let mut shielded_ctx = ctx.namada.shielded_mut().await;

    let viewing_key = zip32::ExtendedFullViewingKey::from(&target_spending_key.into())
        .fvk
        .vk;

    let Some(balance) = shielded_ctx
        .compute_shielded_balance(&viewing_key)
        .await
        .map_err(|e| QueryError::ShieldedContext(e.to_string()))?
    else {
        return Ok(None);
    };

    let total_balance = shielded_ctx
        .decode_combine_sum_to_epoch(client, balance, masp_epoch)
        .await
        .0
        .get(&token_address);

    Ok(Some(total_balance.into()))
}

pub async fn get_block_height(ctx: &Ctx, retry_config: RetryConfig) -> Result<Height, QueryError> {
    let block = tryhard::retry_fn(|| rpc::query_block(&ctx.namada.client))
        .with_config(retry_config)
        .on_retry(|attempt, _, error| {
            let error = error.to_string();
            async move {
                tracing::info!("Retry {} due to {}...", attempt, error);
            }
        })
        .await
        .map_err(QueryError::Rpc)?
        .expect("Block should exist");
    Ok(block.height.into())
}

pub async fn wait_block_settlement(ctx: &Ctx, height: Height, retry_config: RetryConfig) {
    loop {
        if let Ok(current_height) = get_block_height(ctx, retry_config).await {
            if current_height > height {
                break;
            } else {
                tracing::info!(
                    "Waiting for block settlement at height: {}, currently at: {}",
                    height,
                    current_height
                );
            }
        }
        sleep(Duration::from_secs(2)).await
    }
}

pub async fn get_epoch(ctx: &Ctx, retry_config: RetryConfig) -> Result<Epoch, QueryError> {
    tryhard::retry_fn(|| rpc::query_epoch(&ctx.namada.client))
        .with_config(retry_config)
        .on_retry(|attempt, _, error| {
            let error = error.to_string();
            async move {
                tracing::info!("Retry {} due to {}...", attempt, error);
            }
        })
        .await
        .map(|epoch| epoch.into())
        .map_err(QueryError::Rpc)
}

pub async fn get_masp_epoch(ctx: &Ctx, retry_config: RetryConfig) -> Result<MaspEpoch, QueryError> {
    tryhard::retry_fn(|| rpc::query_masp_epoch(&ctx.namada.client))
        .with_config(retry_config)
        .on_retry(|attempt, _, error| {
            let error = error.to_string();
            async move {
                tracing::info!("Retry {} due to {}...", attempt, error);
            }
        })
        .await
        .map_err(QueryError::Rpc)
}

pub async fn get_masp_epoch_at_height(
    ctx: &Ctx,
    height: Height,
    retry_config: RetryConfig,
) -> Result<MaspEpoch, QueryError> {
    let epoch = tryhard::retry_fn(|| rpc::query_epoch_at_height(&ctx.namada.client, height.into()))
        .with_config(retry_config)
        .on_retry(|attempt, _, error| {
            let error = error.to_string();
            async move {
                tracing::info!("Retry {} due to {}...", attempt, error);
            }
        })
        .await
        .map_err(QueryError::Rpc)?
        .expect("Epoch should exist");
    let key = namada_sdk::parameters::storage::get_masp_epoch_multiplier_key();
    let masp_epoch_multiplier =
        tryhard::retry_fn(|| rpc::query_storage_value(&ctx.namada.client, &key))
            .with_config(retry_config)
            .on_retry(|attempt, _, error| {
                let error = error.to_string();
                async move {
                    tracing::info!("Retry {} due to {}...", attempt, error);
                }
            })
            .await
            .map_err(QueryError::Rpc)?;

    MaspEpoch::try_from_epoch(epoch, masp_epoch_multiplier)
        .map_err(|e| QueryError::Convert(e.to_string()))
}

pub async fn get_bond(
    ctx: &Ctx,
    source: &Alias,
    validator: &str,
    epoch: Epoch,
    retry_config: RetryFutureConfig<ExponentialBackoff, NoOnRetry>,
) -> Result<token::Amount, QueryError> {
    let wallet = ctx.namada.wallet.read().await;
    let source_address = wallet
        .find_address(&source.name)
        .ok_or_else(|| QueryError::Wallet(format!("No source address: {}", source.name)))?;
    let validator_address =
        Address::from_str(validator).expect("ValidatorAddress should be converted");
    let epoch = namada_sdk::state::Epoch::from(epoch);

    tryhard::retry_fn(|| {
        rpc::get_bond_amount_at(
            &ctx.namada.client,
            &source_address,
            &validator_address,
            epoch.next().next(),
        )
    })
    .with_config(retry_config)
    .on_retry(|attempt, _, error| {
        let error = error.to_string();
        async move {
            tracing::info!("Retry {} due to {}...", attempt, error);
        }
    })
    .await
    .map_err(QueryError::Rpc)
}

pub async fn get_rewards(
    ctx: &Ctx,
    source: &Alias,
    validator: &str,
    epoch: Epoch,
    retry_config: RetryFutureConfig<ExponentialBackoff, NoOnRetry>,
) -> Result<token::Amount, QueryError> {
    let wallet = ctx.namada.wallet.read().await;
    let source_address = wallet
        .find_address(&source.name)
        .ok_or_else(|| QueryError::Wallet(format!("No source address: {}", source.name)))?;
    let source = Some(source_address.into_owned());
    let validator_address =
        Address::from_str(validator).expect("ValidatorAddress should be converted");
    let epoch = Some(namada_sdk::state::Epoch::from(epoch));

    tryhard::retry_fn(|| {
        rpc::query_rewards(&ctx.namada.client, &source, &validator_address, &epoch)
    })
    .with_config(retry_config)
    .on_retry(|attempt, _, error| {
        let error = error.to_string();
        async move {
            tracing::info!("Retry {} due to {}...", attempt, error);
        }
    })
    .await
    .map_err(QueryError::Rpc)
}

pub async fn shielded_sync_with_retry(
    ctx: &Ctx,
    source: &Alias,
    height: Option<Height>,
    with_indexer: bool,
    retry_config: RetryConfig,
) -> Result<(), QueryError> {
    let with = if with_indexer { "indexer" } else { "node" };
    match tryhard::retry_fn(|| shielded_sync(ctx, height, with_indexer))
        .with_config(retry_config)
        .on_retry(|attempt, _, error| {
            let error = error.to_string();
            async move {
                tracing::info!("Retry {} due to {}...", attempt, error);
            }
        })
        .await
    {
        Ok(_) => {
            tracing::info!(
                "First shielded sync ({with}) for {} was successful",
                source.name
            );
            return Ok(());
        }
        Err(e) => {
            tracing::error!(
                "First shielded sync ({with}) for {} failed: {e}",
                source.name
            );
        }
    }

    // Retry shielded sync without indexer
    match tryhard::retry_fn(|| shielded_sync(ctx, height, false))
        .with_config(retry_config)
        .on_retry(|attempt, _, error| {
            let error = error.to_string();
            async move {
                tracing::info!("Retry {} due to {}...", attempt, error);
            }
        })
        .await
    {
        Ok(_) => {
            tracing::info!("Second shielded sync (node) was successful",);
            Ok(())
        }
        Err(e) => {
            tracing::error!("First shielded sync (node) failed: {e}");
            Err(QueryError::ShieldedSync(e.to_string()))
        }
    }
}

async fn shielded_sync(
    ctx: &Ctx,
    height: Option<Height>,
    with_indexer: bool,
) -> Result<(), QueryError> {
    let now = Instant::now();
    tracing::info!("Started shielded sync (using indexer: {})...", with_indexer);

    let wallet = ctx.namada.wallet.read().await;
    let vks = wallet
        .get_viewing_keys()
        .iter()
        .map(|(alias, vk)| {
            let birthday = wallet.find_birthday(alias).cloned();
            DatedKeypair::new(vk.as_viewing_key(), birthday)
        })
        .collect::<Vec<_>>();
    drop(wallet);

    let mut shielded_ctx = ctx.namada.shielded_mut().await;

    let task_env = MaspLocalTaskEnv::new(4).map_err(|e| QueryError::ShieldedSync(e.to_string()))?;
    let shutdown_signal = install_shutdown_signal(true);
    let enable_wait = height.is_some();
    let height = height.map(|h| h.into());
    tracing::info!("Using height with shielded sync: {height:?}");

    let res = if with_indexer {
        let client = reqwest::Client::builder()
            .connect_timeout(time::Duration::from_secs(60))
            .build()
            .expect("Client should be built");
        let masp_client = IndexerMaspClient::new(
            client,
            Url::parse(&ctx.masp_indexer_url).unwrap(),
            false,
            20,
        );

        let config = ShieldedSyncConfig::builder()
            .client(masp_client)
            .fetched_tracker(DevNullProgressBar)
            .scanned_tracker(DevNullProgressBar)
            .applied_tracker(DevNullProgressBar)
            .shutdown_signal(shutdown_signal)
            .wait_for_last_query_height(enable_wait)
            .build();

        shielded_ctx
            .sync(task_env, config, height, &[], &vks)
            .await
            .map_err(|e| QueryError::ShieldedSync(e.to_string()))
    } else {
        let masp_client = LedgerMaspClient::new(ctx.namada.clone_client(), 10);

        let config = ShieldedSyncConfig::builder()
            .client(masp_client)
            .fetched_tracker(DevNullProgressBar)
            .scanned_tracker(DevNullProgressBar)
            .applied_tracker(DevNullProgressBar)
            .shutdown_signal(shutdown_signal)
            .wait_for_last_query_height(enable_wait)
            .build();

        shielded_ctx
            .sync(task_env, config, height, &[], &vks)
            .await
            .map_err(|e| QueryError::ShieldedSync(e.to_string()))
    };

    if res.is_err() {
        // revert the shielded context
        shielded_ctx.load().await;
    }

    tracing::info!(
        "Done shielded sync (took {}s, with indexer: {with_indexer})!",
        now.elapsed().as_secs(),
    );

    res
}

pub async fn get_proposals(
    ctx: &Ctx,
    last_proposal_id: Option<ProposalId>,
) -> Result<HashMap<ProposalId, (Epoch, Epoch)>, QueryError> {
    let mut proposals = HashMap::new();
    let mut proposal_id = last_proposal_id.map(|id| id + 1).unwrap_or_default();
    while let Some(proposal) = rpc::query_proposal_by_id(&ctx.namada.client, proposal_id)
        .await
        .map_err(QueryError::Rpc)?
    {
        proposals.insert(
            proposal_id,
            (proposal.voting_start_epoch.0, proposal.voting_end_epoch.0),
        );

        proposal_id += 1;
    }

    Ok(proposals)
}

pub async fn get_vote_results(
    ctx: &Ctx,
    target: &Alias,
    proposal_id: ProposalId,
    retry_config: RetryConfig,
) -> Result<Vec<ProposalVote>, QueryError> {
    let wallet = ctx.namada.wallet.read().await;
    let target_address = wallet
        .find_address(&target.name)
        .ok_or_else(|| QueryError::Wallet(format!("No target address: {}", target.name)))?
        .into_owned();
    drop(wallet);

    let votes = tryhard::retry_fn(|| rpc::query_proposal_votes(&ctx.namada.client, proposal_id))
        .with_config(retry_config)
        .on_retry(|attempt, _, error| {
            let error = error.to_string();
            async move {
                tracing::info!("Retry {} due to {}...", attempt, error);
            }
        })
        .await
        .map_err(QueryError::Rpc)?;

    let votes = votes
        .into_iter()
        .filter_map(|vote| {
            if vote.delegator == target_address {
                Some(vote.data)
            } else {
                None
            }
        })
        .collect();

    Ok(votes)
}
