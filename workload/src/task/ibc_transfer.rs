use cosmrs::Any;
use namada_sdk::args::{self, TxBuilder};
use namada_sdk::ibc::convert_masp_tx_to_ibc_memo;
use namada_sdk::ibc::core::host::types::identifiers::ChannelId;
use namada_sdk::masp_primitives;
use namada_sdk::masp_primitives::transaction::components::sapling::builder::RngBuildParams;
use namada_sdk::masp_primitives::zip32::PseudoExtendedKey;
use namada_sdk::signing::SigningData;
use namada_sdk::tx::Tx;
use namada_sdk::Namada;
use namada_sdk::{TransferSource, TransferTarget};
use rand::rngs::OsRng;
use typed_builder::TypedBuilder;

use crate::check::{self, Check};
use crate::constants::IBC_TIMEOUT_HEIGHT_OFFSET;
use crate::context::Ctx;
use crate::error::TaskError;
use crate::state::State;
use crate::task::{TaskContext, TaskSettings};
use crate::types::{Alias, Amount, Height, MaspEpoch};
use crate::utils::{
    base_denom, build_cosmos_ibc_transfer, cosmos_denom_hash, execute_tx, gen_shielding_tx,
    get_balance, get_block_height, get_ibc_packet_sequence, get_shielded_balance, get_token,
    ibc_denom, is_ibc_transfer_successful, is_native_denom, is_recv_packet, retry_config,
    shielded_sync_with_retry, wait_block_settlement, wrapper_tx, RetryConfig,
};

#[derive(Clone, Debug, TypedBuilder)]
pub struct IbcTransferSend {
    source: Alias,
    receiver: Alias,
    denom: String,
    amount: Amount,
    src_channel_id: ChannelId,
    dest_channel_id: ChannelId,
    settings: TaskSettings,
}

impl TaskContext for IbcTransferSend {
    fn name(&self) -> String {
        "ibc-transfer-send".to_string()
    }

    fn summary(&self) -> String {
        format!(
            "ibc-transfer-send/{}/{}/'{}'/{}",
            self.source.name, self.receiver.name, self.denom, self.amount
        )
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        let mut bparams = RngBuildParams::new(OsRng);

        let wallet = ctx.namada.wallet.read().await;

        let source_address = wallet
            .find_address(&self.source.name)
            .ok_or_else(|| TaskError::Wallet(format!("No source address: {}", self.source.name)))?;
        let (token_address, amount) = get_token(ctx, &self.denom, self.amount).await?;

        let source = TransferSource::Address(source_address.into_owned());
        let mut tx_builder = ctx.namada.new_ibc_transfer(
            source,
            self.receiver.name.clone(),
            token_address,
            amount,
            self.src_channel_id.clone(),
            None,
        );
        let mut signing_keys = vec![];
        for signer in &self.settings.signers {
            let public_key = wallet
                .find_public_key(&signer.name)
                .map_err(|e| TaskError::Wallet(e.to_string()))?;
            signing_keys.push(public_key)
        }
        tx_builder = tx_builder.signing_keys(signing_keys);
        drop(wallet);

        let wrapper = wrapper_tx(
            ctx,
            Some(self.settings.gas_payer.clone()),
            Some(self.settings.gas_limit),
        )
        .await;
        tx_builder = tx_builder.wrap_it(Some(wrapper));

        let (transfer_tx, signing_data, _) = tx_builder
            .build(&ctx.namada, &mut bparams)
            .await
            .map_err(|e| TaskError::BuildTx(e.to_string()))?;

        Ok((transfer_tx, signing_data, tx_builder.tx))
    }

    async fn execute(&self, ctx: &Ctx) -> Result<Height, TaskError> {
        let retry_config = retry_config();
        let (tx, signing_data, tx_args) = self.build_tx(ctx).await?;

        let start_height = get_block_height(ctx, retry_config)
            .await
            .unwrap_or_default();

        let height = match execute_tx(ctx, tx, signing_data, &tx_args).await {
            Ok(height) => height,
            Err(e) => {
                wait_block_settlement(ctx, start_height, retry_config).await;
                return Err(e);
            }
        };

        // Wait for the IBC transfer completion
        let sequence = get_ibc_packet_sequence(
            ctx,
            &self.source,
            &self.receiver,
            height,
            true,
            retry_config,
        )
        .await?;
        if is_ibc_transfer_successful(
            ctx,
            &self.src_channel_id,
            &self.dest_channel_id,
            sequence.into(),
            retry_config,
        )
        .await?
        {
            Ok(height)
        } else {
            // The destination rejected the transfer or the packet timed out
            let err = format!(
                "Sending token failed: {} {} from {} to {}",
                self.amount, self.denom, self.source.name, self.receiver.name
            );
            Err(TaskError::IbcTransfer(err))
        }
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

        Ok(vec![source_check])
    }

    fn update_state(&self, state: &mut State) {
        if is_native_denom(&self.denom) {
            state.decrease_balance(&self.source, self.amount);
            state.increase_foreign_balance(&self.receiver, self.amount);
        } else {
            state.decrease_ibc_balance(&self.source, &self.denom, self.amount);
        }
    }
}

#[derive(Clone, Debug, TypedBuilder)]
pub struct IbcTransferRecv {
    sender: Alias,
    target: Alias,
    denom: String,
    amount: Amount,
    src_channel_id: ChannelId,
    dest_channel_id: ChannelId,
    settings: TaskSettings,
}

impl TaskContext for IbcTransferRecv {
    fn name(&self) -> String {
        "ibc-transfer-recv".to_string()
    }

    fn summary(&self) -> String {
        format!(
            "ibc-transfer-recv/{}/{}/'{}'/{}",
            self.sender.name, self.target.name, self.denom, self.amount
        )
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, _ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        unreachable!("Namada tx shouldn't be built")
    }

    async fn execute(&self, ctx: &Ctx) -> Result<Height, TaskError> {
        let retry_config = retry_config();

        let height = self.execute_cosmos_tx(ctx).await?;

        // Check the packet receiving on Namada
        let sequence =
            get_ibc_packet_sequence(ctx, &self.sender, &self.target, height, false, retry_config)
                .await?;
        let (is_successful, recv_height) = is_recv_packet(
            ctx,
            &self.src_channel_id,
            &self.dest_channel_id,
            sequence.into(),
            retry_config,
        )
        .await?;
        if is_successful {
            wait_block_settlement(ctx, recv_height, retry_config).await;
            Ok(recv_height)
        } else {
            // Receiving failed or timed out
            let err = format!(
                "Receiving token failed: {} {} from {} to {}",
                self.amount, self.denom, self.sender.name, self.target.name
            );
            Err(TaskError::IbcTransfer(err))
        }
    }

    async fn build_cosmos_tx(&self, ctx: &Ctx) -> Result<Any, TaskError> {
        let wallet = ctx.namada.wallet.read().await;
        let target_address = wallet
            .find_address(&self.target.name)
            .ok_or_else(|| TaskError::Wallet(format!("No source address: {}", self.target.name)))?
            .into_owned();

        let denom = if is_native_denom(&self.denom) {
            self.denom.clone()
        } else {
            let base_token = base_denom(&self.denom);
            let token_address = wallet.find_address(&base_token).ok_or_else(|| {
                TaskError::Wallet(format!("No native token address: {base_token}",))
            })?;
            let denom = self.denom.replace(&base_token, &token_address.to_string());
            cosmos_denom_hash(&denom)
        };
        drop(wallet);

        let namada_timeout_height =
            get_block_height(ctx, retry_config()).await? + IBC_TIMEOUT_HEIGHT_OFFSET;

        let any_msg = build_cosmos_ibc_transfer(
            &self.sender.name,
            &target_address.to_string(),
            &denom,
            self.amount,
            &self.src_channel_id,
            namada_timeout_height,
            None,
        );

        Ok(any_msg)
    }

    async fn build_checks(
        &self,
        ctx: &Ctx,
        retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        let recv_denom = if is_native_denom(&self.denom) {
            ibc_denom(&self.dest_channel_id, &self.denom)
        } else {
            base_denom(&self.denom)
        };
        let (_, pre_balance) = get_balance(ctx, &self.target, &recv_denom, retry_config).await?;
        let target_check = Check::BalanceTarget(
            check::balance_target::BalanceTarget::builder()
                .target(self.target.clone())
                .pre_balance(pre_balance)
                .denom(recv_denom)
                .amount(self.amount)
                .build(),
        );

        Ok(vec![target_check])
    }

    fn update_state(&self, state: &mut State) {
        if self.denom == ibc_denom(&self.dest_channel_id, &Alias::nam().name) {
            // receiving NAM
            state.increase_balance(&self.target, self.amount);
            state.decrease_foreign_balance(&self.sender, self.amount);
        } else {
            let ibc_denom = ibc_denom(&self.dest_channel_id, &self.denom);
            state.increase_ibc_balance(&self.target, &ibc_denom, self.amount);
        }
    }
}

#[derive(Clone, Debug, TypedBuilder)]
pub struct IbcShieldingTransfer {
    sender: Alias,
    target: Alias,
    denom: String,
    amount: Amount,
    src_channel_id: ChannelId,
    dest_channel_id: ChannelId,
    settings: TaskSettings,
}

impl TaskContext for IbcShieldingTransfer {
    fn name(&self) -> String {
        "ibc-shielding-transfer".to_string()
    }

    fn summary(&self) -> String {
        format!(
            "ibc-shielding-transfer/{}/{}/'{}'/{}",
            self.sender.name, self.target.name, self.denom, self.amount
        )
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, _ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        unreachable!("Namada tx shouldn't be built")
    }

    async fn execute(&self, ctx: &Ctx) -> Result<Height, TaskError> {
        let retry_config = retry_config();

        let height = self.execute_cosmos_tx(ctx).await?;

        // Need to check the packet receipt before checking
        let sequence = get_ibc_packet_sequence(
            ctx,
            &self.sender,
            &Alias::masp(),
            height,
            false,
            retry_config,
        )
        .await?;
        let (is_successful, recv_height) = is_recv_packet(
            ctx,
            &self.src_channel_id,
            &self.dest_channel_id,
            sequence.into(),
            retry_config,
        )
        .await?;
        if is_successful {
            wait_block_settlement(ctx, recv_height, retry_config).await;
            // Returns Namada height where the packet was received
            Ok(recv_height)
        } else {
            // Receiving failed or timed out
            let err = format!(
                "Receiving token failed: {} {} from {} to {}",
                self.amount, self.denom, self.sender.name, self.target.name
            );
            Err(TaskError::IbcTransfer(err))
        }
    }

    async fn build_cosmos_tx(&self, ctx: &Ctx) -> Result<Any, TaskError> {
        let wallet = ctx.namada.wallet.read().await;
        let target_payment_address =
            *wallet.find_payment_addr(&self.target.name).ok_or_else(|| {
                TaskError::Wallet(format!("No payment address: {}", self.target.name))
            })?;
        let masp_alias = Alias::masp();
        let masp_address = wallet
            .find_address(&masp_alias.name)
            .ok_or_else(|| TaskError::Wallet(format!("No MASP address: {}", masp_alias.name)))?
            .into_owned();

        let (denom_on_cosmos, ibc_denom) = if is_native_denom(&self.denom) {
            (self.denom.clone(), self.denom.clone())
        } else {
            let base_token = base_denom(&self.denom);
            let token_address = wallet.find_address(&base_token).ok_or_else(|| {
                TaskError::Wallet(format!("No native token address: {base_token}",))
            })?;
            let denom = self.denom.replace(&base_token, &token_address.to_string());
            (cosmos_denom_hash(&denom), denom)
        };
        drop(wallet);

        let shielding_tx =
            gen_shielding_tx(ctx, target_payment_address, &ibc_denom, self.amount).await?;
        let memo = convert_masp_tx_to_ibc_memo(&shielding_tx);

        let namada_timeout_height =
            get_block_height(ctx, retry_config()).await? + IBC_TIMEOUT_HEIGHT_OFFSET;

        let any_msg = build_cosmos_ibc_transfer(
            &self.sender.name,
            &masp_address.to_string(),
            &denom_on_cosmos,
            self.amount,
            &self.src_channel_id,
            namada_timeout_height,
            Some(&memo),
        );

        Ok(any_msg)
    }

    async fn build_checks(
        &self,
        ctx: &Ctx,
        retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        shielded_sync_with_retry(ctx, &self.target, None, false, retry_config).await?;

        let recv_denom = if is_native_denom(&self.denom) {
            ibc_denom(&self.dest_channel_id, &self.denom)
        } else {
            base_denom(&self.denom)
        };
        let pre_balance = get_shielded_balance(ctx, &self.target, &recv_denom, retry_config)
            .await?
            .unwrap_or_default();
        let target_check = Check::BalanceShieldedTarget(
            check::balance_shielded_target::BalanceShieldedTarget::builder()
                .target(self.target.clone())
                .pre_balance(pre_balance)
                .denom(recv_denom)
                .amount(self.amount)
                .build(),
        );

        Ok(vec![target_check])
    }

    fn update_state(&self, state: &mut State) {
        if self.denom == ibc_denom(&self.dest_channel_id, &Alias::nam().name) {
            // receiving NAM
            state.increase_masp_balance(&self.target, self.amount);
            state.decrease_foreign_balance(&self.sender, self.amount);
        } else {
            let ibc_denom = ibc_denom(&self.dest_channel_id, &self.denom);
            state.increase_ibc_balance(&self.target, &ibc_denom, self.amount);
        }
    }
}

#[derive(Clone, Debug, TypedBuilder)]
pub struct IbcUnshieldingTransfer {
    source: Alias,
    receiver: Alias,
    denom: String,
    amount: Amount,
    src_channel_id: ChannelId,
    dest_channel_id: ChannelId,
    epoch: MaspEpoch,
    settings: TaskSettings,
}

impl TaskContext for IbcUnshieldingTransfer {
    fn name(&self) -> String {
        "ibc-unshielding-transfer".to_string()
    }

    fn summary(&self) -> String {
        format!(
            "ibc-unshielding-transfer/{}/{}/'{}'/{}",
            self.source.name, self.receiver.name, self.denom, self.amount
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
        let source = TransferSource::ExtendedKey(pseudo_spending_key_from_spending_key);

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

        // use the original transparent address for testing
        let refund_target = wallet
            .find_address(self.source.base().name)
            .ok_or_else(|| {
                TaskError::Wallet(format!(
                    "No transparent source address: {}",
                    self.source.base().name
                ))
            })?
            .into_owned();
        drop(wallet);

        let (token_address, amount) = get_token(ctx, &self.denom, self.amount).await?;

        let mut tx_builder = ctx.namada.new_ibc_transfer(
            source,
            self.receiver.name.clone(),
            token_address,
            amount,
            self.src_channel_id.clone(),
            None,
        );
        tx_builder.gas_spending_key = gas_spending_key;
        tx_builder.refund_target = Some(TransferTarget::Address(refund_target));

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
        tx_builder = tx_builder.wrap_it(Some(wrapper));

        // signing key isn't needed for unshielding transfer

        let (transfer_tx, signing_data, _) = tx_builder
            .build(&ctx.namada, &mut bparams)
            .await
            .map_err(|e| TaskError::BuildTx(e.to_string()))?;

        Ok((transfer_tx, signing_data, tx_builder.tx))
    }

    async fn execute(&self, ctx: &Ctx) -> Result<Height, TaskError> {
        let height = self.execute_shielded_tx(ctx, self.epoch).await?;

        // Wait for the IBC transfer completion
        let sequence = get_ibc_packet_sequence(
            ctx,
            &self.source.base(),
            &self.receiver,
            height,
            true,
            retry_config(),
        )
        .await?;
        if is_ibc_transfer_successful(
            ctx,
            &self.src_channel_id,
            &self.dest_channel_id,
            sequence.into(),
            retry_config(),
        )
        .await?
        {
            Ok(height)
        } else {
            // The destination rejected the transfer or the packet timed out
            let err = format!(
                "Sending token failed: {} {} from {} to {}",
                self.amount, self.denom, self.source.name, self.receiver.name
            );
            Err(TaskError::IbcTransfer(err))
        }
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

        Ok(vec![source_check])
    }

    fn update_state(&self, state: &mut State) {
        if is_native_denom(&self.denom) {
            state.decrease_masp_balance(&self.source, self.amount);
            state.increase_foreign_balance(&self.receiver, self.amount);
        } else {
            state.decrease_ibc_balance(&self.source, &self.denom, self.amount);
        }
    }
}
