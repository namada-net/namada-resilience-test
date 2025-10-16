use std::path::PathBuf;
use std::str::FromStr;

use namada_sdk::args::{
    self, DeviceTransport, GenIbcShieldingTransfer, IbcShieldingTransferAsset, InputAmount, Query,
    TxBuilder, Wrapper,
};
use namada_sdk::collections::HashSet;
use namada_sdk::control_flow::time;
use namada_sdk::error::{Error as NamadaError, TxSubmitError};
use namada_sdk::hash::Hash;
use namada_sdk::ibc::core::host::types::identifiers::PortId;
use namada_sdk::key::common;
use namada_sdk::masp_primitives::transaction::Transaction as MaspTransaction;
use namada_sdk::rpc::{self, InnerTxResult, TxResponse};
use namada_sdk::signing::{default_sign, SigningData};
use namada_sdk::token;
use namada_sdk::tx::data::{compute_inner_tx_hash, GasLimit, TxType};
use namada_sdk::tx::{
    self, either, gen_ibc_shielding_transfer, save_initialized_accounts, ProcessTxResponse, Tx,
    TxCommitments, TX_REVEAL_PK,
};
use namada_sdk::{Namada, PaymentAddress};

use crate::constants::DEFAULT_GAS_LIMIT;
use crate::context::Ctx;
use crate::error::TaskError;
use crate::task::TaskSettings;
use crate::types::{Alias, Amount, Height};
use crate::utils::is_native_denom;

fn get_tx_errors(
    cmts: HashSet<TxCommitments>,
    wrapper_hash: Option<Hash>,
    tx_response: &ProcessTxResponse,
) -> Option<String> {
    if let ProcessTxResponse::Applied(result) = tx_response {
        if let Some(batch) = &result.batch {
            tracing::info!("batch result: {:#?}", batch);

            let errors = cmts
                .iter()
                .filter_map(|cmt| {
                    batch
                        .get_inner_tx_result(wrapper_hash.as_ref(), either::Right(cmt))
                        .map(|res| match res.as_ref() {
                            Ok(res) => serde_json::to_string(&res.vps_result.errors)
                                .expect("errors should be json"),
                            Err(e) => e.to_string(),
                        })
                })
                .collect::<Vec<_>>()
                .join(", ");

            return Some(errors);
        } else {
            return Some(result.info.clone());
        }
    }
    None
}

pub async fn wrapper_tx(
    ctx: &Ctx,
    wrapper_fee_payer: Option<Alias>,
    gas_limit: Option<u64>,
) -> Wrapper {
    let wallet = ctx.namada.wallet.read().await;
    let nam = wallet
        .find_address("nam")
        .expect("Native token should be present.")
        .into_owned();
    let fee_payer_alias = wrapper_fee_payer.unwrap_or(Alias::faucet()).name;
    let fee_payer_pk = wallet
        .find_public_key(fee_payer_alias)
        .expect("Faucet should exist");
    Wrapper {
        broadcast_only: false,
        fee_amount: None,
        fee_token: nam.clone(),
        wrapper_fee_payer: Some(fee_payer_pk),
        gas_limit: GasLimit::from(gas_limit.unwrap_or(DEFAULT_GAS_LIMIT)),
    }
}

async fn default_tx_arg(ctx: &Ctx) -> args::Tx {
    let wrapper = wrapper_tx(ctx, None, None).await;

    args::Tx {
        dry_run: None,
        dump_tx: None,
        output_folder: None,
        force: false,
        ledger_address: namada_sdk::tendermint_rpc::Url::from_str("http://127.0.0.1:26657")
            .unwrap(),
        initialized_account_alias: None,
        wallet_alias_force: false,
        wrap_tx: Some(wrapper),
        expiration: Default::default(),
        chain_id: None,
        signing_keys: vec![],
        tx_reveal_code_path: PathBuf::from(TX_REVEAL_PK),
        password: None,
        memo: None,
        use_device: false,
        device_transport: DeviceTransport::default(),
    }
}

pub async fn build_reveal_pk(
    ctx: &Ctx,
    public_key: common::PublicKey,
) -> Result<(Tx, SigningData, args::Tx), TaskError> {
    let wrapper = wrapper_tx(ctx, None, None).await;

    let mut reveal_pk_tx_builder = ctx
        .namada
        .new_reveal_pk(public_key.clone())
        .signing_keys(vec![public_key]);
    reveal_pk_tx_builder = reveal_pk_tx_builder.wrap_it(Some(wrapper));

    let (reveal_tx, signing_data) = reveal_pk_tx_builder
        .build(&ctx.namada)
        .await
        .map_err(|e| TaskError::BuildTx(e.to_string()))?;

    Ok((reveal_tx, signing_data, reveal_pk_tx_builder.tx))
}

pub async fn execute_reveal_pk(
    ctx: &Ctx,
    public_key: common::PublicKey,
) -> Result<Height, TaskError> {
    let (tx, signing_data, tx_args) = build_reveal_pk(ctx, public_key).await?;

    execute_tx(ctx, tx, signing_data, &tx_args).await
}

pub async fn merge_tx(
    ctx: &Ctx,
    txs: Vec<(Tx, SigningData)>,
    settings: &TaskSettings,
) -> Result<(Tx, SigningData, args::Tx), TaskError> {
    if txs.is_empty() {
        return Err(TaskError::BuildTx("Empty tx batch".to_string()));
    }
    let tx_args = default_tx_arg(ctx).await;

    let wallet = ctx.namada.wallet.read().await;
    let faucet_alias = Alias::faucet();
    let gas_payer_pk = wallet
        .find_public_key(faucet_alias.name)
        .map_err(|e| TaskError::Wallet(e.to_string()))?;
    drop(wallet);

    let (tx, signing_datas) = if txs.len() == 1 {
        let (tx, signing_data) = txs[0].clone();
        (tx, signing_data)
    } else {
        let (mut tx, signing_wrapper_data) =
            tx::build_batch(txs.clone()).map_err(|e| TaskError::BuildTx(e.to_string()))?;
        tx.header.atomic = true;

        let mut wrapper = tx.header.wrapper().expect("wrapper should exist");
        wrapper.gas_limit = GasLimit::from(settings.gas_limit);
        wrapper.pk = gas_payer_pk.clone();
        tx.header.tx_type = TxType::Wrapper(Box::new(wrapper));

        (tx, SigningData::Wrapper(signing_wrapper_data))
    };

    tracing::info!("Built batch with {} txs.", txs.len());

    Ok((tx, signing_datas, tx_args))
}

pub(crate) async fn execute_tx(
    ctx: &Ctx,
    tx: Tx,
    signing_data: SigningData,
    tx_args: &args::Tx,
) -> Result<Height, TaskError> {
    let mut tx = tx;

    let is_batch = tx.commitments().len() > 1;
    ctx.namada
        .sign(&mut tx, tx_args, signing_data, default_sign, ())
        .await
        .expect("unable to sign tx");

    if is_batch {
        let gas_payer_sk = ctx
            .namada
            .wallet_mut()
            .await
            .find_secret_key(Alias::faucet().name, None)
            .map_err(|e| TaskError::Wallet(e.to_string()))?;
        tx.sign_wrapper(gas_payer_sk);
    }

    let first_cmt = tx
        .first_commitments()
        .expect("Commitments should exist")
        .clone();
    let cmts = tx.commitments().clone();
    let tx_hash = tx.header_hash().to_string();
    let wrapper_hash = tx.wrapper_hash();

    let tx_response = match ctx.namada.submit(tx, tx_args).await {
        Ok(response) => response,
        Err(NamadaError::Tx(TxSubmitError::AppliedTimeout)) => {
            retry_tx_status_check(ctx, tx_args, &tx_hash, wrapper_hash, &cmts).await?
        }
        Err(e) => return Err(TaskError::Broadcast(e)),
    };

    let (height, gas_used) =
        if let ProcessTxResponse::Applied(TxResponse {
            height, gas_used, ..
        }) = tx_response
        {
            tracing::info!("Used gas: {gas_used}");
            (height.0, gas_used)
        } else {
            return Err(TaskError::TxResp(format!(
                "Unexpected tx response type: {tx_response:?}"
            )));
        };

    if tx_response
        .is_applied_and_valid(wrapper_hash.as_ref(), &first_cmt)
        .is_none()
    {
        let errors = get_tx_errors(cmts, wrapper_hash, &tx_response).unwrap_or_default();
        if u64::from(gas_used) != 0 {
            return Err(TaskError::Execution {
                err: errors,
                height,
            });
        } else {
            return Err(TaskError::InsufficientGas {
                err: errors,
                height,
            });
        }
    }

    Ok(height)
}

async fn retry_tx_status_check(
    ctx: &Ctx,
    tx_args: &args::Tx,
    tx_hash: &str,
    wrapper_hash: Option<Hash>,
    cmts: &HashSet<TxCommitments>,
) -> Result<ProcessTxResponse, TaskError> {
    tracing::info!("Retrying to check if tx was applied...");

    let tx_query = rpc::TxEventQuery::Applied(tx_hash);
    let deadline = time::Instant::now() + time::Duration::from_secs(600);
    let events = rpc::query_tx_status(&ctx.namada, tx_query, deadline)
        .await
        .map_err(TaskError::Broadcast)?;
    let tx_response = TxResponse::from_events(events);

    // add initialized accounts when init-account
    for cmt in cmts {
        if let Some(InnerTxResult::Success(result)) = tx_response.batch_result().get(
            &compute_inner_tx_hash(wrapper_hash.as_ref(), either::Right(cmt)),
        ) {
            save_initialized_accounts(&ctx.namada, tx_args, result.initialized_accounts.clone())
                .await;
        }
    }

    Ok(ProcessTxResponse::Applied(tx_response))
}

/// Generate a MASP transaction for a transfer from Cosmos to Namada
pub async fn gen_shielding_tx(
    ctx: &Ctx,
    target: PaymentAddress,
    denom: &str,
    amount: Amount,
) -> Result<MaspTransaction, TaskError> {
    let denominated_amount = if is_native_denom(denom) {
        // cosmos token
        token::DenominatedAmount::new(token::Amount::from_u64(amount), 0u8.into())
    } else {
        // receiving NAM
        token::DenominatedAmount::new(token::Amount::from_u64(amount), 6u8.into())
    };
    let args = GenIbcShieldingTransfer {
        query: Query {
            ledger_address: "http://127.0.0.1:27657".parse().expect("dummy address"),
        },
        output_folder: None,
        target,
        asset: IbcShieldingTransferAsset::LookupNamadaAddress {
            token: denom.to_string(),
            port_id: PortId::transfer(),
            channel_id: ctx.namada_channel_id.clone(),
        },
        amount: InputAmount::Validated(denominated_amount),
        expiration: args::TxExpiration::NoExpiration,
        frontend_sus_fee: None,
    };
    Ok(gen_ibc_shielding_transfer(&ctx.namada, args)
        .await
        .map_err(|e| TaskError::BuildTx(e.to_string()))?
        .expect("MASP tx should be generated"))
}
