use std::{collections::HashSet, env, fs, io::Write, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use futures::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use starknet_rust::{
    accounts::{Account, ConnectedAccount, ExecutionEncoding, SingleOwnerAccount},
    core::{
        types::{
            BlockId, BlockTag, Call, ContractClass, ExecutionResult, Felt, FunctionCall, TransactionFinalityStatus, TypedData,
        },
        utils::get_selector_from_name,
    },
    providers::{JsonRpcClient, Provider, Url, jsonrpc::HttpTransport},
    signers::{LocalWallet, SigningKey},
};

mod legacy;

type Rpc = JsonRpcClient<HttpTransport>;
type Wallet = SingleOwnerAccount<Rpc, LocalWallet>;

const STRK: Felt =
    Felt::from_hex_unchecked("0x04718f5a0fc34cc1af16a1cdee98ffb20c31f5cd61d6ab07201858f4287c938d");
const AVNU_EXCHANGE: Felt =
    Felt::from_hex_unchecked("0x04270219d365d6b017231b52e92b3fb5d7c8378b05e9abc97724537a80e93b0f");
const DEFAULT_RPC: &str = "https://api.cartridge.gg/x/starknet/mainnet";
const RPC_LIST_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/rpc_list.json");
const TOKEN_LIST: &str = "https://prod-api.ekubo.org/tokens?chainId=23448594291968334&pageSize=1000";
const AVNU: &str = "https://starknet.api.avnu.fi/swap/v3";
const PAYMASTER: &str = "https://starknet.paymaster.avnu.fi";
const MAX_SLIPPAGE: f64 = 0.1;

const ARGENT_0_5_0: Felt =
    Felt::from_hex_unchecked("0x073414441639dcd11d1846f287650a00c60c416b9d3ba45d31c651672125b2c2");
const BRAAVOS_1_2_0: Felt =
    Felt::from_hex_unchecked("0x03957f9f5a1cbfe918cedc2015c85200ca51a5f7506ecb6de98a5207b759bf8a");

// Sources: argentlabs/argent-contracts-starknet deployments/account.txt, myBraavos/braavos-account-cairo releases.
// (from, to, argent-style `upgrade(class, data)`, label)
const UPGRADES: &[(Felt, Felt, bool, &str)] = &[
    (
        Felt::from_hex_unchecked("0x01a736d6ed154502257f02b1ccdf4d9d1089f80811cd6acad48e6b6a9d1f2003"),
        ARGENT_0_5_0,
        true,
        "Argent 0.3.0 -> 0.5.0",
    ),
    (
        Felt::from_hex_unchecked("0x029927c8af6bccf3f6fda035981e765a7bdbf18a2dc0d630494f8758aa908e2b"),
        ARGENT_0_5_0,
        true,
        "Argent 0.3.1 -> 0.5.0",
    ),
    (
        Felt::from_hex_unchecked("0x036078334509b514626504edc9fb252328d1a240e4e948bef8d0c08dff45927f"),
        ARGENT_0_5_0,
        true,
        "Argent 0.4.0 -> 0.5.0",
    ),
    (
        Felt::from_hex_unchecked("0x00816dd0297efc55dc1e7559020a3a825e81ef734b558f03c83325d4da7e6253"),
        BRAAVOS_1_2_0,
        false,
        "Braavos 1.0.0 -> 1.2.0",
    ),
    (
        Felt::from_hex_unchecked("0x02c8c7e6fbcfb3e8e15a46648e8914c6aa1fc506fc1e7fb3d1e19630716174bc"),
        BRAAVOS_1_2_0,
        false,
        "Braavos 1.1.0 -> 1.2.0",
    ),
];

struct Config {
    account: Felt,
    key: SigningKey,
    rpc: Option<String>,
    relayer: Option<legacy::Relayer>,
    env_file: PathBuf,
    slippage: f64,
    execute: bool,
    send_to: Option<Felt>,
}

#[derive(Deserialize)]
struct RpcList {
    default: String,
    fallbacks: Vec<String>,
}

#[derive(Deserialize)]
struct Token {
    symbol: String,
    address: String,
    decimals: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Quote {
    quote_id: String,
    buy_amount: String,
    sell_amount_in_usd: Option<f64>,
    routes: Vec<Route>,
}

#[derive(Deserialize)]
struct Route {
    name: String,
    percent: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AvnuCall {
    contract_address: String,
    entrypoint: String,
    calldata: Vec<String>,
}

#[derive(Deserialize)]
struct Built {
    calls: Vec<AvnuCall>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = config()?;
    let rpc = find_working_rpc(cfg.rpc.as_deref()).await?;
    let http = reqwest::Client::builder().user_agent("swap-to-strk/0.1").build()?;
    let provider = JsonRpcClient::new(HttpTransport::new(rpc));
    let chain_id = provider.chain_id().await?;

    println!("\n━━━ ACCOUNT ━━━");
    println!("  Address:  {:#x}", cfg.account);
    println!("  Mode:     {}", if cfg.execute { "✓ LIVE (will execute)" } else { "⚪ Dry-run (planning only)" });

    if !legacy::migrate(&provider, chain_id, &cfg).await? {
        println!();
        return Ok(());
    }

    let encoding = match provider
        .get_class_at(latest(), cfg.account)
        .await
        .context("failed to read account class; account may not be deployed")?
    {
        ContractClass::Legacy(_) => ExecutionEncoding::Legacy,
        ContractClass::Sierra(_) => ExecutionEncoding::New,
    };
    let account = SingleOwnerAccount::new(
        provider,
        LocalWallet::from_signing_key(cfg.key.clone()),
        cfg.account,
        chain_id,
        encoding,
    );

    let class = account.provider().get_class_hash_at(latest(), cfg.account).await?;
    let known = class == ARGENT_0_5_0 || class == BRAAVOS_1_2_0 || UPGRADES.iter().any(|u| u.0 == class);
    let note = if known { "" } else { " (unrecognized, no upgrade path)" };
    println!("  Class:    {class:#x}{note}");
    check_signer(account.provider(), cfg.account, &cfg.key, "STARKNET_PRIVATE_KEY").await?;
    if let Err(e) = legacy::refund_relayer(account.provider(), chain_id, &cfg).await {
        println!("  ⚠ Relayer refund failed: {e}");
    }

    if let Some((label, call)) = pending_upgrade(&account, class).await? {
        println!("\n━━━ UPGRADE (PRIORITY) ━━━");
        println!("  {label}");
        let calls = [call];
        if can_pay(&account, &calls).await {
            if cfg.execute {
                send(&account, calls.to_vec()).await?;
            } else {
                println!("  → Will execute upgrade");
            }
        } else {
            println!("  → Upgrade via AVNU Paymaster");
            paymaster(&http, &account, &cfg.key, &calls, &["upgrade".into()], cfg.execute).await?;
        }
    }

    let tokens: Vec<Token> = http.get(TOKEN_LIST).send().await?.error_for_status()?.json().await?;
    let (mut calls, mut entrypoints) = (Vec::new(), Vec::new());

    println!("\n━━━ HOLDINGS ━━━");
    for (token, address, amount) in holdings(account.provider(), &tokens, cfg.account).await {
        println!("  • {} {}", units(amount, token.decimals), token.symbol);
        if address == STRK {
            continue;
        }
        match swap_calls(&http, address, amount, cfg.account, cfg.slippage).await {
            Ok(c) => {
                println!("    → Swap queued");
                for (name, call) in c {
                    entrypoints.push(name);
                    calls.push(call);
                }
            }
            Err(e) => println!("    ⚠ Skipped: {e}"),
        }
    }

    println!("\n━━━ EXECUTION ━━━");
    if calls.is_empty() {
        println!("  ✓ No swaps needed");
    } else if can_pay(&account, &calls).await {
        println!("  → Swaps execute directly (gas from STRK balance)");
        if cfg.execute {
            send(&account, calls).await?;
        }
    } else {
        println!("  → Swaps use AVNU Paymaster (gas from swap output)");
        paymaster(&http, &account, &cfg.key, &calls, &entrypoints, cfg.execute).await?;
    }

    if let Some(to) = cfg.send_to {
        println!("\n━━━ TRANSFER ━━━");
        send_all_strk(&account, to, cfg.execute).await?;
    }

    println!("\n━━━ COMPLETE ━━━");
    if cfg.execute {
        println!("  ✓ Transactions executed\n");
    } else {
        println!("  ✓ Plan preview, nothing sent. Run without --dry-run to execute.\n");
    }
    Ok(())
}

fn config() -> Result<Config> {
    let env_file = dotenvy::dotenv().unwrap_or_else(|_| {
        let fallback = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"));
        dotenvy::from_path(&fallback).ok();
        fallback
    });
    let (mut slippage, mut execute, mut send_to) = (0.01, true, None);
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => execute = false,
            "--slippage" => slippage = args.next().context("--slippage needs a value")?.parse()?,
            "--send-to" => {
                let to = args.next().context("--send-to needs an address")?;
                send_to = Some(felt(&to).ok().filter(|f| *f != Felt::ZERO).context("--send-to is not a valid address")?);
            }
            other => bail!("unknown argument {other}"),
        }
    }
    ensure!(slippage > 0.0 && slippage <= MAX_SLIPPAGE, "--slippage must be in (0, {MAX_SLIPPAGE}]");

    let var = |name: &str| env::var(name).ok().filter(|v| !v.trim().is_empty());
    let address = |name: &str| -> Result<Felt> {
        felt(&var(name).with_context(|| format!("{name} is not set"))?)
            .with_context(|| format!("{name} is not a valid address"))
    };
    let key = |name: &str| -> Result<SigningKey> {
        let raw = var(name).with_context(|| format!("{name} is not set"))?;
        Ok(SigningKey::from_secret_scalar(
            Felt::from_hex(&raw).ok().with_context(|| format!("{name} is not a valid hex key"))?,
        ))
    };

    let pair = |address_var: &str, key_var: &str| -> Result<(Felt, SigningKey)> {
        let (address, key) = (address(address_var)?, key(key_var)?);
        ensure!(
            address != key.verifying_key().scalar(),
            "{address_var} is the public key of {key_var}; use the account address shown in your wallet instead"
        );
        Ok((address, key))
    };

    let relayer = match (var("RELAYER_ACCOUNT"), var("RELAYER_PRIVATE_KEY")) {
        (None, None) => None,
        (Some(_), Some(_)) => {
            let (address, key) = pair("RELAYER_ACCOUNT", "RELAYER_PRIVATE_KEY")?;
            Some(legacy::Relayer { address, key })
        }
        _ => bail!("set both RELAYER_ACCOUNT and RELAYER_PRIVATE_KEY, or neither"),
    };

    let (account, key) = pair("STARKNET_ACCOUNT", "STARKNET_PRIVATE_KEY")?;
    ensure!(send_to != Some(account), "--send-to is the account itself");
    Ok(Config {
        account,
        key,
        rpc: var("STARKNET_RPC"),
        relayer,
        env_file,
        slippage,
        execute,
        send_to,
    })
}

async fn find_working_rpc(preferred: Option<&str>) -> Result<Url> {
    let mut urls: Vec<String> = preferred.map(String::from).into_iter().collect();
    if let Ok(content) = fs::read_to_string(RPC_LIST_FILE) {
        let list: RpcList = serde_json::from_str(&content).context("invalid rpc_list.json")?;
        urls.push(list.default);
        urls.extend(list.fallbacks);
    }
    urls.push(DEFAULT_RPC.into());

    let mut seen = HashSet::new();
    for raw in urls.into_iter().filter(|u| seen.insert(u.clone())) {
        let Ok(url) = Url::parse(&raw) else { continue };
        let provider = JsonRpcClient::new(HttpTransport::new(url.clone()));
        if matches!(tokio::time::timeout(Duration::from_secs(8), provider.chain_id()).await, Ok(Ok(_))) {
            return Ok(url);
        }
        eprintln!("  RPC unavailable, skipping: {}", url.host_str().unwrap_or(&raw));
    }
    bail!("no working RPC in STARKNET_RPC, rpc_list.json or the built-in default")
}

async fn pending_upgrade(account: &Wallet, class: Felt) -> Result<Option<(&'static str, Call)>> {
    let Some(&(_, to, argent, label)) = UPGRADES.iter().find(|u| u.0 == class) else {
        return Ok(None);
    };
    if account.provider().get_class(latest(), to).await.is_err() {
        println!("  ⚠ {label} available but target class is not declared");
        return Ok(None);
    }
    let mut calldata = vec![to];
    if argent {
        calldata.push(Felt::ZERO);
    }
    Ok(Some((label, Call { to: account.address(), selector: selector("upgrade"), calldata })))
}

async fn can_pay(account: &Wallet, calls: &[Call]) -> bool {
    let strk = balance(account.provider(), STRK, account.address()).await.unwrap_or(0);
    println!("    Your balance:   {} STRK", units(strk, 18));
    match account.execute_v3(calls.to_vec()).estimate_fee().await {
        Ok(fee) => {
            let required = fee.overall_fee.saturating_mul(3);
            println!("    Gas estimate:   {} STRK", units(fee.overall_fee, 18));
            println!("    Min required:   {} STRK (gas + 2x buffer)", units(required, 18));
            strk >= required
        }
        Err(e) => {
            println!("    ⚠ Gas estimation failed: {e}");
            false
        }
    }
}

// Gas bounds come from an estimate that includes signature validation; wallets that skip it can underprice
// accounts with an expensive `__validate__` (e.g. migrated Argent 0.5.0) and fail with "Out of gas".
async fn send_all_strk(wallet: &Wallet, to: Felt, execute: bool) -> Result<()> {
    let funds = balance(wallet.provider(), STRK, wallet.address()).await.context("can't read STRK balance")?;
    let transfer = |amount: u128| Call { to: STRK, selector: selector("transfer"), calldata: vec![to, amount.into(), Felt::ZERO] };
    let fee = wallet.execute_v3(vec![transfer(funds)]).estimate_fee().await?;
    let margin = |v: u128| v.saturating_mul(3) / 2;
    let (l1, l2, l1_data) = (
        margin(fee.l1_gas_consumed.into()) as u64,
        margin(fee.l2_gas_consumed.into()) as u64,
        margin(fee.l1_data_gas_consumed.into()) as u64,
    );
    let (p1, p2, p1_data) = (margin(fee.l1_gas_price), margin(fee.l2_gas_price), margin(fee.l1_data_gas_price));
    let max_fee = u128::from(l1) * p1 + u128::from(l2) * p2 + u128::from(l1_data) * p1_data;
    let Some(amount) = funds.checked_sub(max_fee).filter(|a| *a > 0) else {
        println!("  Nothing to send after gas ({} STRK available)", units(funds, 18));
        return Ok(());
    };
    println!("  Sending {} STRK to {to:#x} (gas up to {} STRK)", units(amount, 18), units(max_fee, 18));
    if !execute {
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
    wait(wallet.provider(), tx.transaction_hash).await
}

// Some accounts (e.g. Braavos) skip signature checks during fee estimation, so a wrong key would only surface
// at send time. SNIP-6 `is_valid_signature` returns 'VALID' (Cairo 0 accounts: 1) for a signer's signature.
async fn check_signer(provider: &Rpc, address: Felt, key: &SigningKey, key_var: &str) -> Result<()> {
    const VALID: Felt = Felt::from_hex_unchecked("0x56414c4944");
    let hash = selector("swap-to-strk signer check");
    let sig = key.sign(&hash)?;
    let call = FunctionCall {
        contract_address: address,
        entry_point_selector: selector("is_valid_signature"),
        calldata: vec![hash, Felt::TWO, sig.r, sig.s],
    };
    match provider.call(call, latest()).await {
        Ok(r) if matches!(r.first(), Some(v) if *v == VALID || *v == Felt::ONE) => Ok(()),
        Err(e) if e.to_string().contains("not found") => Ok(()),
        _ => {
            println!("\n━━━ KEY DOES NOT MATCH ━━━");
            println!("  {key_var} can't sign for {address:#x}. What to do:");
            println!("  1. Wrong key: in your wallet, export the private key of this exact account (each account");
            println!("     has its own key) and put it in {key_var}. The address stays the account address (0x…),");
            println!("     not the public key.");
            println!("  2. Wrong address: if the key belongs to another account, use that account's address instead.");
            println!("  3. Extra protection on: 2FA, guardian/Shield, multisig or a hardware signer need a second");
            println!("     signature. Turn it off in the wallet app, wait for it to take effect, then rerun.");
            bail!("{key_var} is not a valid signer for {address:#x}")
        }
    }
}

async fn send(account: &Wallet, calls: Vec<Call>) -> Result<()> {
    let tx = account.execute_v3(calls).send().await?;
    wait(account.provider(), tx.transaction_hash).await
}

async fn wait(provider: &Rpc, hash: Felt) -> Result<()> {
    print!("    Tx {hash:#x} … ");
    std::io::stdout().flush().ok();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let Ok(r) = provider.get_transaction_receipt(hash).await else { continue };
        if let ExecutionResult::Reverted { reason } = r.receipt.execution_result() {
            println!("✗ reverted");
            bail!("{reason}");
        }
        // Pre-confirmed receipts arrive before the block closes; `latest` state isn't updated until then.
        if *r.receipt.finality_status() != TransactionFinalityStatus::PreConfirmed {
            println!("✓ confirmed");
            return Ok(());
        }
    }
    bail!("transaction not confirmed after 5 minutes")
}

async fn holdings<'a>(provider: &Rpc, tokens: &'a [Token], owner: Felt) -> Vec<(&'a Token, Felt, u128)> {
    let mut held: Vec<_> = stream::iter(tokens)
        .map(|t| async move {
            let address = Felt::from_hex(&t.address).ok()?;
            let amount = balance(provider, address, owner).await?;
            (amount > 0).then_some((t, address, amount))
        })
        .buffer_unordered(16)
        .filter_map(|x| async move { x })
        .collect()
        .await;
    held.sort_by(|a, b| a.0.symbol.cmp(&b.0.symbol));
    held
}

async fn balance(provider: &Rpc, token: Felt, owner: Felt) -> Option<u128> {
    for name in ["balance_of", "balanceOf"] {
        let call = FunctionCall { contract_address: token, entry_point_selector: selector(name), calldata: vec![owner] };
        if let Ok(r) = provider.call(call, latest()).await {
            return match r.as_slice() {
                [low] => u128::try_from(*low).ok(),
                [low, high] => u256(*low, *high),
                _ => None,
            };
        }
    }
    None
}

async fn swap_calls(
    http: &reqwest::Client,
    sell: Felt,
    amount: u128,
    taker: Felt,
    slippage: f64,
) -> Result<Vec<(String, Call)>> {
    let quotes: Vec<Quote> = http
        .get(format!("{AVNU}/quotes"))
        .query(&[
            ("sellTokenAddress", hex(sell)),
            ("buyTokenAddress", hex(STRK)),
            ("sellAmount", format!("{amount:#x}")),
            ("takerAddress", hex(taker)),
            ("size", "1".into()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let quote = quotes.into_iter().next().context("no route")?;
    let out = u128::from_str_radix(quote.buy_amount.trim_start_matches("0x"), 16)?;
    let route: Vec<_> = quote.routes.iter().map(|r| format!("{} {:.0}%", r.name, r.percent * 100.0)).collect();
    println!(
        "    → {} STRK (${:.4}) via {}",
        units(out, 18),
        quote.sell_amount_in_usd.unwrap_or_default(),
        route.join(", ")
    );

    let built: Built = http
        .post(format!("{AVNU}/build"))
        .json(&json!({ "quoteId": quote.quote_id, "takerAddress": hex(taker), "slippage": slippage, "includeApprove": true }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let calls = built
        .calls
        .iter()
        .map(|c| {
            let calldata = c.calldata.iter().map(|x| felt(x)).collect::<Result<_>>()?;
            Ok((c.entrypoint.clone(), Call { to: felt(&c.contract_address)?, selector: selector(&c.entrypoint), calldata }))
        })
        .collect::<Result<Vec<_>>>()?;
    let min_out = out - (out as f64 * slippage * 1.01) as u128;
    check_swap_calls(&calls, sell, amount, taker, min_out)?;
    Ok(calls)
}

// AVNU's API output is executed by our account, so only allow approve(exchange) + multi_route_swap to us.
fn check_swap_calls(calls: &[(String, Call)], sell: Felt, amount: u128, taker: Felt, min_out: u128) -> Result<()> {
    let mut swaps = 0;
    for (_, c) in calls {
        let d = &c.calldata;
        if c.to == sell && c.selector == selector("approve") {
            ensure!(
                d.len() == 3 && d[0] == AVNU_EXCHANGE && u256(d[1], d[2]) == Some(amount),
                "AVNU returned an unexpected approve"
            );
        } else if c.to == AVNU_EXCHANGE && c.selector == selector("multi_route_swap") {
            ensure!(
                d.len() > 10
                    && d[0] == sell
                    && u256(d[1], d[2]) == Some(amount)
                    && d[3] == STRK
                    && u256(d[6], d[7]).is_some_and(|m| m >= min_out)
                    && d[8] == taker
                    && d[9] == Felt::ZERO,
                "AVNU returned swap parameters that don't match the quote"
            );
            swaps += 1;
        } else {
            bail!("AVNU returned an unexpected call to {:#x}", c.to);
        }
    }
    ensure!(swaps == 1, "AVNU returned {swaps} swap calls");
    Ok(())
}

async fn paymaster(
    http: &reqwest::Client,
    account: &Wallet,
    key: &SigningKey,
    calls: &[Call],
    entrypoints: &[String],
    execute: bool,
) -> Result<()> {
    let user = hex(account.address());
    let params = json!({ "version": "0x1", "fee_mode": { "mode": "default", "gas_token": hex(STRK) } });
    let invoke: Vec<Value> = calls
        .iter()
        .map(|c| json!({ "to": hex(c.to), "selector": hex(c.selector), "calldata": c.calldata.iter().map(|x| hex(*x)).collect::<Vec<_>>() }))
        .collect();
    let built = pm_rpc(
        http,
        "paymaster_buildTransaction",
        json!({ "transaction": { "type": "invoke", "invoke": { "user_address": user, "calls": invoke } }, "parameters": params }),
    )
    .await?;
    let max_fee = u128::try_from(felt(built["fee"]["suggested_max_fee_in_strk"].as_str().context("no fee in response")?)?)?;
    println!("    Paymaster fee up to {} STRK", units(max_fee, 18));
    let typed = built["typed_data"].clone();
    check_typed_calls(&typed, calls, max_fee)?;
    if !execute {
        return Ok(());
    }

    let mut names = entrypoints.to_vec();
    names.push("transfer".into());
    let hash = serde_json::from_value::<TypedData>(with_selector_names(&typed, &names)?)?.message_hash(account.address())?;
    let sig = key.sign(&hash)?;
    let res = pm_rpc(
        http,
        "paymaster_executeTransaction",
        json!({ "transaction": { "type": "invoke", "invoke": { "user_address": user, "typed_data": typed, "signature": [hex(sig.r), hex(sig.s)] } }, "parameters": params }),
    )
    .await?;
    wait(account.provider(), felt(res["transaction_hash"].as_str().context("no tx hash in response")?)?).await
}

// The paymaster may only append one STRK transfer (its fee) to our calls.
fn check_typed_calls(typed: &Value, ours: &[Call], max_fee: u128) -> Result<()> {
    let msg = &typed["message"];
    let list = msg
        .get("Calls")
        .or_else(|| msg.get("calls"))
        .and_then(Value::as_array)
        .context("typed data has no calls")?;
    let theirs = list.iter().map(typed_call).collect::<Result<Vec<_>>>()?;
    let same = |a: &Call, b: &Call| a.to == b.to && a.selector == b.selector && a.calldata == b.calldata;
    ensure!(
        theirs.len() >= ours.len() && theirs.iter().zip(ours).all(|(a, b)| same(a, b)),
        "paymaster changed the calls"
    );
    match &theirs[ours.len()..] {
        [] => Ok(()),
        [fee] => {
            let amount = match fee.calldata.as_slice() {
                [_, low, high] => u256(*low, *high),
                _ => None,
            };
            ensure!(
                fee.to == STRK && fee.selector == selector("transfer") && amount.is_some_and(|a| a <= max_fee),
                "unexpected paymaster fee call"
            );
            Ok(())
        }
        _ => bail!("paymaster added unexpected calls"),
    }
}

// starknet-rust hashes `Selector` as an entrypoint name; SNIP-9 v2 typed data carries it as hex.
fn with_selector_names(typed: &Value, names: &[String]) -> Result<Value> {
    let mut typed = typed.clone();
    if let Some(calls) = typed["message"].get_mut("Calls").and_then(Value::as_array_mut) {
        for c in calls {
            let hex = felt(c["Selector"].as_str().context("malformed selector")?)?;
            let name = names.iter().find(|n| selector(n) == hex).context("unknown selector in typed data")?;
            c["Selector"] = name.as_str().into();
        }
    }
    Ok(typed)
}

fn typed_call(v: &Value) -> Result<Call> {
    let field = |k: &str| v.get(k).or_else(|| v.get(k.to_lowercase())).context("malformed typed call");
    let str_felt = |x: &Value| felt(x.as_str().context("expected string")?);
    Ok(Call {
        to: str_felt(field("To")?)?,
        selector: str_felt(field("Selector")?)?,
        calldata: field("Calldata")?.as_array().context("calldata")?.iter().map(str_felt).collect::<Result<_>>()?,
    })
}

async fn pm_rpc(http: &reqwest::Client, method: &str, params: Value) -> Result<Value> {
    let res: Value = http
        .post(PAYMASTER)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(err) = res.get("error") {
        bail!("{method}: {err}");
    }
    res.get("result").cloned().context("empty paymaster response")
}

fn latest() -> BlockId {
    BlockId::Tag(BlockTag::Latest)
}

fn selector(name: &str) -> Felt {
    get_selector_from_name(name).expect("ascii entrypoint")
}

fn felt(s: &str) -> Result<Felt> {
    Ok(if s.starts_with("0x") { Felt::from_hex(s)? } else { Felt::from_dec_str(s)? })
}

fn u256(low: Felt, high: Felt) -> Option<u128> {
    (high == Felt::ZERO).then(|| u128::try_from(low).ok()).flatten()
}

fn hex(f: Felt) -> String {
    format!("{f:#x}")
}

fn units(v: u128, decimals: u32) -> String {
    let d = decimals as usize;
    let s = format!("{v:0>w$}", w = d + 1);
    let (int, frac) = s.split_at(s.len() - d);
    match frac.trim_end_matches('0') {
        "" => int.to_string(),
        frac => format!("{int}.{frac}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ETH: Felt = Felt::from_hex_unchecked("0x49d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7");
    const ME: Felt = Felt::from_hex_unchecked("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde");
    const AMOUNT: u128 = 0x121e6c485ac000;
    const MIN: u128 = 0x100556fe653998d1eb;

    fn avnu_calls(beneficiary: Felt) -> Vec<(String, Call)> {
        let approve = Call { to: ETH, selector: selector("approve"), calldata: vec![AVNU_EXCHANGE, AMOUNT.into(), Felt::ZERO] };
        let swap = Call {
            to: AVNU_EXCHANGE,
            selector: selector("multi_route_swap"),
            calldata: vec![
                ETH, AMOUNT.into(), Felt::ZERO, STRK, Felt::from(0x102ec47a801b260000u128), Felt::ZERO,
                MIN.into(), Felt::ZERO, beneficiary, Felt::ZERO, Felt::ZERO, Felt::ONE,
            ],
        };
        vec![("approve".into(), approve), ("multi_route_swap".into(), swap)]
    }

    #[test]
    fn accepts_genuine_avnu_swap() {
        check_swap_calls(&avnu_calls(ME), ETH, AMOUNT, ME, MIN).unwrap();
    }

    #[test]
    fn rejects_foreign_beneficiary() {
        assert!(check_swap_calls(&avnu_calls(Felt::ONE), ETH, AMOUNT, ME, MIN).is_err());
    }

    #[test]
    fn rejects_low_min_out() {
        assert!(check_swap_calls(&avnu_calls(ME), ETH, AMOUNT, ME, MIN + 1).is_err());
    }

    #[test]
    fn rejects_injected_transfer() {
        let mut calls = avnu_calls(ME);
        calls.push(("transfer".into(), Call { to: ETH, selector: selector("transfer"), calldata: vec![Felt::ONE, AMOUNT.into(), Felt::ZERO] }));
        assert!(check_swap_calls(&calls, ETH, AMOUNT, ME, MIN).is_err());
    }
}
