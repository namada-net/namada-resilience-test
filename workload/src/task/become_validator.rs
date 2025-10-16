use namada_sdk::args::{self, TxBuilder};
use namada_sdk::key::{RefTo, SchemeType};
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
use crate::types::{Alias, CommissionChange, CommissionRate};
use crate::utils::{wrapper_tx, RetryConfig};

#[derive(Clone, Debug, TypedBuilder)]
pub struct BecomeValidator {
    source: Alias,
    consensus_alias: Alias,
    eth_cold_alias: Alias,
    eth_hot_alias: Alias,
    protocol_alias: Alias,
    commission_rate: CommissionRate,
    commission_max_change: CommissionChange,
    settings: TaskSettings,
}

impl TaskContext for BecomeValidator {
    fn name(&self) -> String {
        "become-validator".to_string()
    }

    fn summary(&self) -> String {
        format!("become-validator/{}", self.source.name)
    }

    fn task_settings(&self) -> Option<&TaskSettings> {
        Some(&self.settings)
    }

    async fn build_tx(&self, ctx: &Ctx) -> Result<(Tx, SigningData, args::Tx), TaskError> {
        let mut wallet = ctx.namada.wallet.write().await;

        let consensus_pk = wallet
            .gen_store_secret_key(
                SchemeType::Ed25519,
                Some(self.consensus_alias.name.clone()),
                true,
                None,
                &mut OsRng,
            )
            .expect("Key generation should not fail.")
            .1
            .ref_to();

        let eth_cold_pk = wallet
            .gen_store_secret_key(
                SchemeType::Secp256k1,
                Some(self.eth_cold_alias.name.clone()),
                true,
                None,
                &mut OsRng,
            )
            .expect("Key generation should not fail.")
            .1
            .ref_to();

        let eth_hot_pk = wallet
            .gen_store_secret_key(
                SchemeType::Secp256k1,
                Some(self.eth_hot_alias.name.clone()),
                true,
                None,
                &mut OsRng,
            )
            .expect("Key generation should not fail.")
            .1
            .ref_to();

        let protocol_key = wallet
            .gen_store_secret_key(
                SchemeType::Ed25519,
                Some(self.protocol_alias.name.clone()),
                true,
                None,
                &mut OsRng,
            )
            .expect("Key generation should not fail.")
            .1
            .ref_to();

        let source_address = wallet
            .find_address(&self.source.name)
            .ok_or_else(|| TaskError::Wallet(format!("No source address: {}", self.source.name)))?;
        wallet
            .save()
            .map_err(|e| TaskError::Wallet(format!("Failed to save the wallet: {e}")))?;

        let mut become_validator_tx_builder = ctx
            .namada
            .new_become_validator(
                source_address.into_owned(),
                self.commission_rate,
                self.commission_max_change,
                consensus_pk,
                eth_cold_pk,
                eth_hot_pk,
                protocol_key,
                "test@test.it".to_string(),
            )
            .wallet_alias_force(true);

        let mut signing_keys = vec![];
        for signer in &self.settings.signers {
            let public_key = wallet
                .find_public_key(&signer.name)
                .map_err(|e| TaskError::Wallet(e.to_string()))?;
            signing_keys.push(public_key)
        }
        become_validator_tx_builder = become_validator_tx_builder.signing_keys(signing_keys);
        drop(wallet);

        let wrapper = wrapper_tx(
            ctx,
            Some(self.settings.gas_payer.clone()),
            Some(self.settings.gas_limit),
        )
        .await;
        become_validator_tx_builder = become_validator_tx_builder.wrap_it(Some(wrapper));

        let (become_validator, signing_data) = become_validator_tx_builder
            .build(&ctx.namada)
            .await
            .map_err(|e| TaskError::BuildTx(e.to_string()))?;

        Ok((
            become_validator,
            signing_data,
            become_validator_tx_builder.tx,
        ))
    }

    async fn build_checks(
        &self,
        _ctx: &Ctx,
        _retry_config: RetryConfig,
    ) -> Result<Vec<Check>, TaskError> {
        Ok(vec![Check::IsValidatorAccount(
            check::validator_account::ValidatorAccount::builder()
                .target(self.source.clone())
                .build(),
        )])
    }

    fn update_state(&self, state: &mut State) {
        state.set_established_as_validator(&self.source)
    }
}
