// Recovery for Argent/Ready accounts without `__validate__`, ported from github.com/argentlabs/upgrade-v0-account.

use std::{fs, io::Write, path::Path};

use anyhow::{Result, bail, ensure};
use starknet_rust::{
    accounts::{Account, AccountFactory, ConnectedAccount, ExecutionEncoding, OpenZeppelinAccountFactory, SingleOwnerAccount},
    core::{
        crypto::compute_hash_on_elements,
        types::{
            BroadcastedTransaction, Call, ContractClass, ExecuteInvocation, Felt, FunctionCall, SimulationFlag,
            StarknetError, TransactionTrace,
        },
        utils::{get_contract_address, get_storage_var_address},
    },
    providers::{Provider, ProviderError},
    signers::{LocalWallet, SigningKey},
};

use crate::{Config, Rpc, STRK, Wallet, balance, latest, selector, units, wait};

const PROXY_V0_2_2: Felt =
    Felt::from_hex_unchecked("0x25ec026985a3bf9d0cc1fe17326b245dfdc3ff89b8fde106542a3ea56c5a918");
const IMPL_V0_2_1: Felt =
    Felt::from_hex_unchecked("0x6a1776964b9f991c710bfe910b8b37578b32b26a7dffd1669a1a59ac94bf82f");
const IMPL_V0_2_2: Felt =
    Felt::from_hex_unchecked("0x3e327de1c40540b98d05cbcb13552008e36f0ec8d61d46956d2f9752c294328");
const IMPL_V0_2_3_0: Felt =
    Felt::from_hex_unchecked("0x01a7820094feaf82d53f53f214b81292d717e7bb9a92bb2488092cd306f3993f");
const IMPL_V0_2_3_1: Felt =
    Felt::from_hex_unchecked("0x33434ad846cdd5f23eb73ff09fe6fddd568284a0fb7d1be20ee482f044dabe2");
const ARGENT_0_4_0: Felt =
    Felt::from_hex_unchecked("0x036078334509b514626504edc9fb252328d1a240e4e948bef8d0c08dff45927f");
// Argent's helper wrapping the Starknet 0.14 `meta_tx_v0` syscall (mainnet only).
const META_TX_V0: Felt =
    Felt::from_hex_unchecked("0x03e21ab91c0899efc48b6d6ccd09b61fd37766e9b0c3cc968a7655632fbc253c");
const INVOKE_PREFIX: Felt = Felt::from_hex_unchecked("0x696e766f6b65");
const OZ_ACCOUNT: Felt =
    Felt::from_hex_unchecked("0x05b4b537eaa2399e3aa99c4e2e0208ebd6c71bc1467938cd52c798c601e43564");
const MIN_RELAYER_STRK: u128 = 1_000_000_000_000_000_000;
const MIN_REFUND_STRK: u128 = 50_000_000_000_000_000;

enum Stage {
    MetaTxV0,
    DirectV3,
}

pub struct Relayer {
    pub address: Felt,
    pub key: SigningKey,
}

/// Returns true when the account no longer needs legacy handling.
pub async fn migrate(provider: &Rpc, chain_id: Felt, cfg: &Config) -> Result<bool> {
    let (account, key) = (cfg.account, &cfg.key);
    let mut announced = false;
    loop {
        let class = provider.get_class_hash_at(latest(), account).await?;
        if class != PROXY_V0_2_2 {
            return Ok(true);
        }
        let implementation = view(provider, account, "get_implementation").await?;
        let stage = match implementation {
            i if i == IMPL_V0_2_1 || i == IMPL_V0_2_2 => Stage::MetaTxV0,
            i if i == IMPL_V0_2_3_0 || i == IMPL_V0_2_3_1 => Stage::DirectV3,
            other => bail!("legacy proxy with unknown implementation {other:#x}"),
        };

        if !announced {
            announced = true;
            println!("\n━━━ LEGACY ACCOUNT RECOVERY ━━━");
            println!("  Argent/Ready proxy, implementation {}", version(implementation));
            println!("  Path: → 0.2.3.1 (meta_tx_v0) → 0.4.0 (v3) → 0.5.0 + swaps");
            check_owner(provider, account, key).await?;
        }

        match stage {
            Stage::MetaTxV0 => {
                println!("\n  Step 1/2: upgrade to 0.2.3.1 via meta_tx_v0");
                println!("    This account can't send transactions itself, so a relayer account submits it (~0.05 STRK gas).");
                let call = meta_tx_upgrade(provider, chain_id, account, key).await?;
                let Some(relayer) = &cfg.relayer else {
                    let address = create_relayer(&cfg.env_file)?;
                    println!("    ✓ Created relayer account, key saved to {}", cfg.env_file.display());
                    next_steps(address, None);
                    return Ok(false);
                };
                let funds = balance(provider, STRK, relayer.address).await.unwrap_or(0);
                println!("    Relayer: {:#x} ({} STRK)", relayer.address, units(funds, 18));
                if funds < MIN_RELAYER_STRK {
                    next_steps(relayer.address, Some(funds));
                    return Ok(false);
                }
                let Some(wallet) = relayer_account(provider, chain_id, relayer, cfg.execute).await? else {
                    println!("    → Will deploy the relayer and submit the upgrade");
                    return Ok(false);
                };
                send_measured(&wallet, vec![call], cfg.execute).await?;
                if !cfg.execute {
                    return Ok(false);
                }
                let now = view(provider, account, "get_implementation").await?;
                ensure!(
                    now == IMPL_V0_2_3_1,
                    "meta-tx confirmed but implementation is still {now:#x}; the inner __execute__ failed"
                );
            }
            Stage::DirectV3 => {
                println!("\n  Step 2/2: upgrade to Argent 0.4.0 (account pays its own STRK gas)");
                let wallet = SingleOwnerAccount::new(
                    provider.clone(),
                    LocalWallet::from_signing_key(key.clone()),
                    account,
                    chain_id,
                    ExecutionEncoding::Legacy,
                );
                // Data [0] makes the upgrade also drop the proxy.
                let upgrade = Call {
                    to: account,
                    selector: selector("upgrade"),
                    calldata: vec![ARGENT_0_4_0, Felt::ONE, Felt::ZERO],
                };
                send_measured(&wallet, vec![upgrade], cfg.execute).await?;
                if !cfg.execute {
                    return Ok(false);
                }
            }
        }
    }
}

async fn meta_tx_upgrade(provider: &Rpc, chain_id: Felt, account: Felt, key: &SigningKey) -> Result<Call> {
    let nonce = view(provider, account, "get_nonce").await?;
    let execute = selector("__execute__");
    let calldata = vec![
        Felt::ONE,
        account,
        selector("upgrade"),
        Felt::ZERO,
        Felt::ONE,
        Felt::ONE,
        IMPL_V0_2_3_1,
        nonce,
    ];
    let hash = v0_invoke_hash(account, &calldata, chain_id);
    let sig = key.sign(&hash)?;

    let valid = provider
        .call(
            FunctionCall {
                contract_address: account,
                entry_point_selector: selector("is_valid_signature"),
                calldata: vec![hash, Felt::TWO, sig.r, sig.s],
            },
            latest(),
        )
        .await;
    ensure!(valid.is_ok(), "account rejected the v0 signature; wrong private key?");

    let mut outer = vec![account, execute, Felt::from(calldata.len())];
    outer.extend(calldata);
    outer.extend([Felt::TWO, sig.r, sig.s]);
    Ok(Call { to: META_TX_V0, selector: selector("execute_meta_tx_v0"), calldata: outer })
}

fn v0_invoke_hash(account: Felt, calldata: &[Felt], chain_id: Felt) -> Felt {
    compute_hash_on_elements(&[
        INVOKE_PREFIX,
        Felt::ZERO,
        account,
        selector("__execute__"),
        compute_hash_on_elements(calldata),
        Felt::ZERO,
        chain_id,
    ])
}

async fn check_owner(provider: &Rpc, account: Felt, key: &SigningKey) -> Result<()> {
    let signer = storage(provider, account, "_signer").await?;
    let guardian = storage(provider, account, "_guardian").await?;
    ensure!(
        signer == key.verifying_key().scalar(),
        "STARKNET_PRIVATE_KEY does not match the account signer {signer:#x}"
    );
    ensure!(guardian == Felt::ZERO, "account has a guardian ({guardian:#x}); remove it first");
    println!("  ✓ Private key matches signer, no guardian");
    Ok(())
}

async fn relayer_account(provider: &Rpc, chain_id: Felt, relayer: &Relayer, execute: bool) -> Result<Option<Wallet>> {
    let signer = LocalWallet::from_signing_key(relayer.key.clone());
    let encoding = match provider.get_class_at(latest(), relayer.address).await {
        Ok(ContractClass::Legacy(_)) => ExecutionEncoding::Legacy,
        Ok(ContractClass::Sierra(_)) => ExecutionEncoding::New,
        Err(ProviderError::StarknetError(StarknetError::ContractNotFound)) => {
            ensure!(
                oz_address(&relayer.key) == relayer.address,
                "RELAYER_ACCOUNT {:#x} is not deployed",
                relayer.address
            );
            if !execute {
                return Ok(None);
            }
            println!("    Deploying relayer account");
            let factory = OpenZeppelinAccountFactory::new(OZ_ACCOUNT, chain_id, signer.clone(), provider.clone()).await?;
            let tx = factory.deploy_v3(relayer.key.verifying_key().scalar()).send().await?;
            wait(provider, tx.transaction_hash).await?;
            ExecutionEncoding::New
        }
        Err(e) => return Err(e.into()),
    };
    Ok(Some(SingleOwnerAccount::new(provider.clone(), signer, relayer.address, chain_id, encoding)))
}

// Fee estimation runs as a query tx, which makes meta_tx_v0 hash the inner v0 tx with the query version and
// reject our signature. So gas is measured with a regular signed simulation and the bounds are set explicitly.
async fn send_measured(wallet: &Wallet, calls: Vec<Call>, execute: bool) -> Result<()> {
    let nonce = wallet.get_nonce().await?;
    let probe = wallet
        .execute_v3(calls.clone())
        .nonce(nonce)
        .l1_gas(0)
        .l1_gas_price(0)
        .l2_gas(500_000_000)
        .l2_gas_price(0)
        .l1_data_gas(10_000)
        .l1_data_gas_price(0)
        .tip(0)
        .prepared()?
        .get_invoke_request(false, false)
        .await?;
    let sim = wallet
        .provider()
        .simulate_transaction(wallet.block_id(), BroadcastedTransaction::Invoke(probe), [SimulationFlag::SkipFeeCharge])
        .await?;
    if let TransactionTrace::Invoke(trace) = &sim.transaction_trace
        && let ExecuteInvocation::Reverted(r) = &trace.execute_invocation
    {
        bail!("transaction would revert: {}", r.revert_reason);
    }
    let fee = sim.fee_estimation;
    println!("    Gas estimate: {} STRK", units(fee.overall_fee, 18));
    if !execute {
        println!("    → Will submit");
        return Ok(());
    }
    let margin = |gas: u64| gas.saturating_mul(3) / 2;
    let tx = wallet
        .execute_v3(calls)
        .nonce(nonce)
        .l1_gas(margin(fee.l1_gas_consumed))
        .l2_gas(margin(fee.l2_gas_consumed))
        .l1_data_gas(margin(fee.l1_data_gas_consumed))
        .send()
        .await?;
    wait(wallet.provider(), tx.transaction_hash).await
}

/// Returns leftover STRK from a relayer this tool generated; user-supplied relayers are never touched.
pub async fn refund_relayer(provider: &Rpc, chain_id: Felt, cfg: &Config) -> Result<()> {
    let Some(relayer) = &cfg.relayer else { return Ok(()) };
    if oz_address(&relayer.key) != relayer.address || relayer.address == cfg.account {
        return Ok(());
    }
    let funds = balance(provider, STRK, relayer.address).await.unwrap_or(0);
    if funds < MIN_REFUND_STRK {
        return Ok(());
    }
    println!("\n━━━ RELAYER REFUND ━━━");
    println!("  Relayer {:#x} holds {} STRK", relayer.address, units(funds, 18));
    let Some(wallet) = relayer_account(provider, chain_id, relayer, cfg.execute).await? else {
        println!("  → Will deploy the relayer and return its STRK to {:#x}", cfg.account);
        return Ok(());
    };

    let transfer = |amount: u128| Call {
        to: STRK,
        selector: selector("transfer"),
        calldata: vec![cfg.account, amount.into(), Felt::ZERO],
    };
    let fee = wallet.execute_v3(vec![transfer(funds)]).estimate_fee().await?;
    let gas = |consumed: u64| consumed.saturating_mul(3) / 2;
    let (l1, l2, l1_data) = (gas(fee.l1_gas_consumed), gas(fee.l2_gas_consumed), gas(fee.l1_data_gas_consumed));
    let (p1, p2, p1_data) = (fee.l1_gas_price * 2, fee.l2_gas_price * 2, fee.l1_data_gas_price * 2);
    let max_fee = u128::from(l1) * p1 + u128::from(l2) * p2 + u128::from(l1_data) * p1_data;
    let Some(amount) = funds.checked_sub(max_fee).filter(|a| *a > 0) else {
        println!("  Nothing left after gas");
        return Ok(());
    };
    println!("  Returning {} STRK to {:#x} (gas up to {} STRK)", units(amount, 18), cfg.account, units(max_fee, 18));
    if !cfg.execute {
        return Ok(());
    }
    let tx = wallet
        .execute_v3(vec![transfer(amount)])
        .l1_gas(l1)
        .l1_gas_price(p1)
        .l2_gas(l2)
        .l2_gas_price(p2)
        .l1_data_gas(l1_data)
        .l1_data_gas_price(p1_data)
        .tip(0)
        .send()
        .await?;
    wait(provider, tx.transaction_hash).await
}

fn oz_address(key: &SigningKey) -> Felt {
    let public = key.verifying_key().scalar();
    get_contract_address(public, OZ_ACCOUNT, &[public], Felt::ZERO)
}

// The private key goes to .env only; nothing but the address is printed.
fn create_relayer(env_file: &Path) -> Result<Felt> {
    let existing = fs::read_to_string(env_file).unwrap_or_default();
    ensure!(
        !existing.lines().any(|l| l.trim_start().starts_with("RELAYER_")),
        "{} has empty RELAYER_* entries; fill them in or remove them",
        env_file.display()
    );
    let key = SigningKey::from_random();
    let address = oz_address(&key);

    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(env_file)?;
    let sep = if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" };
    write!(
        file,
        "{sep}# relayer for legacy account recovery, generated by swap-to-strk\nRELAYER_ACCOUNT={address:#x}\nRELAYER_PRIVATE_KEY={:#x}\n",
        key.secret_scalar()
    )?;
    #[cfg(unix)]
    fs::set_permissions(env_file, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(address)
}

fn next_steps(relayer: Felt, funds: Option<u128>) {
    let run = std::env::args().next().unwrap_or_else(|| "swap-to-strk".into());
    println!("\n━━━ NEXT STEPS ━━━");
    if let Some(funds) = funds {
        println!("  Relayer holds {} STRK; it needs at least 1 STRK.", units(funds, 18));
    }
    println!("  1. Send at least 1 STRK (Starknet mainnet) to the relayer:");
    println!("       {relayer:#x}");
    println!("     Or use one of your funded wallets: set RELAYER_ACCOUNT and RELAYER_PRIVATE_KEY in .env");
    println!("  2. After the transfer confirms, run again:");
    println!("       {run}");
    println!("     It deploys the relayer if new, upgrades this account, then swaps everything to STRK.");
}

async fn view(provider: &Rpc, contract: Felt, name: &str) -> Result<Felt> {
    let call = FunctionCall { contract_address: contract, entry_point_selector: selector(name), calldata: vec![] };
    let out = provider.call(call, latest()).await?;
    out.first().copied().ok_or_else(|| anyhow::anyhow!("{name} returned nothing"))
}

async fn storage(provider: &Rpc, contract: Felt, var: &str) -> Result<Felt> {
    let address = get_storage_var_address(var, &[])?;
    Ok(provider.get_storage_at(contract, address, latest(), None).await?.value())
}

fn version(implementation: Felt) -> &'static str {
    match implementation {
        i if i == IMPL_V0_2_1 => "0.2.1",
        i if i == IMPL_V0_2_2 => "0.2.2",
        i if i == IMPL_V0_2_3_0 => "0.2.3.0",
        _ => "0.2.3.1",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected value from starknet.js v2hash.calculateTransactionHashCommon.
    #[test]
    fn v0_hash_matches_starknet_js() {
        let account = Felt::from_hex_unchecked("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde");
        let calldata = [Felt::ONE, account, selector("upgrade"), Felt::ZERO, Felt::ONE, Felt::ONE, IMPL_V0_2_3_1, Felt::ZERO];
        let chain_id = Felt::from_hex_unchecked("0x534e5f4d41494e");
        assert_eq!(
            v0_invoke_hash(account, &calldata, chain_id),
            Felt::from_hex_unchecked("0x36deb2bf7b23a41d62e876668eef16c594b785a76e18eaa7a39341f3f074813")
        );
    }

    // Expected value from starknet.js hash.calculateContractAddressFromHash.
    #[test]
    fn relayer_address_matches_starknet_js() {
        let key = SigningKey::from_secret_scalar(Felt::from_hex_unchecked("0x1234"));
        assert_eq!(
            oz_address(&key),
            Felt::from_hex_unchecked("0x34057df49bad9d59c98523c4abcf03ea95a17280022ab44259c5a5978fdaff5")
        );
    }
}
