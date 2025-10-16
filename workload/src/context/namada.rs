use std::str::FromStr;

use namada_sdk::tendermint_rpc::{HttpClient, Url};
use namada_sdk::{
    address::{Address, ImplicitAddress},
    args::TxBuilder,
    chain::ChainId,
    io::NullIo,
    key::common::SecretKey,
    masp::{fs::FsShieldedUtils, ShieldedContext},
    rpc, NamadaImpl,
};
use namada_wallet::fs::FsWalletUtils;

use crate::config::AppConfig;
use crate::utils::thread_id;

pub type NamadaCtx = NamadaImpl<HttpClient, FsWalletUtils, FsShieldedUtils, NullIo>;

pub async fn namada_ctx(config: &AppConfig) -> Result<NamadaCtx, String> {
    let base_dir = crate::utils::base_dir();

    let url = Url::from_str(&config.rpc).expect("invalid RPC address");
    let http_client = HttpClient::new(url).unwrap();

    // Setup wallet storage
    let wallet_path = base_dir.join(format!("wallet-{}", thread_id()));
    std::fs::create_dir_all(&wallet_path).expect("Create wallet directory failed");
    let mut wallet = FsWalletUtils::new(wallet_path.clone());
    if wallet_path.join("wallet.toml").exists() {
        wallet.load().expect("Should be able to load the wallet");
    } else {
        // Set the faucet and the native token
        let sk = SecretKey::from_str(&config.faucet_sk).unwrap();
        let public_key = sk.to_public();
        let address = Address::Implicit(ImplicitAddress::from(&public_key));

        wallet
            .insert_keypair("faucet".to_string(), true, sk, None, Some(address), None)
            .unwrap();

        let native_token = rpc::query_native_token(&http_client)
            .await
            .map_err(|e| e.to_string())?;
        wallet.insert_address("nam", native_token, true).unwrap();

        wallet.save().expect("Should be able to save the wallet");
    }

    // Setup shielded context storage
    let shielded_ctx_path = base_dir.join(format!("masp-{}", thread_id()));
    std::fs::create_dir_all(&shielded_ctx_path).expect("Create masp directory failed");
    let mut shielded_ctx = ShieldedContext::new(FsShieldedUtils::new(shielded_ctx_path.clone()));
    if shielded_ctx_path.join("shielded.dat").exists() {
        shielded_ctx.load().await;
    } else {
        shielded_ctx.save().await.unwrap();
    }

    let namada = NamadaImpl::new(http_client, wallet, shielded_ctx.into(), NullIo)
        .await
        .map_err(|e| e.to_string())?;
    let namada = namada.chain_id(ChainId::from_str(&config.chain_id).unwrap());
    Ok(namada)
}

pub fn reconnect(ctx: &mut NamadaCtx, config: &AppConfig) {
    let url = Url::from_str(&config.rpc).expect("invalid RPC address");
    let http_client = HttpClient::new(url).unwrap();

    ctx.client = http_client;
}
