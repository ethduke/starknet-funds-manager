# starknet-funds-manager

Rescue funds from Starknet wallets you already own. `swap-to-strk` upgrades outdated accounts, including legacy Argent/Ready 0.2.x accounts that can no longer send transactions, then swaps every token in the wallet to STRK.

## What it does

- **Upgrades accounts:** Argent 0.3.x/0.4.0 → 0.5.0 and Braavos 1.0/1.1 → 1.2, before any swap.
- **Recovers legacy accounts:** Argent/Ready 0.2.1–0.2.3 accounts (Cairo 0, no `__validate__`) are brought to Argent 0.5.0 using Starknet 0.14's `meta_tx_v0`.
- **Swaps to STRK:** every token balance is swapped through AVNU. Gas comes from the STRK balance, or from the swap output via the AVNU paymaster.

## Quick start

Requires Rust 1.85+ and Starknet mainnet.

```bash
git clone https://github.com/ethduke/starknet-funds-manager
cd starknet-funds-manager/swap-to-strk

cat > .env <<'EOF'
STARKNET_ACCOUNT=0xYOUR_ACCOUNT_ADDRESS
STARKNET_PRIVATE_KEY=0xYOUR_PRIVATE_KEY
EOF
chmod 600 .env

cargo build --release
./target/release/swap-to-strk --dry-run   # preview, sends nothing
./target/release/swap-to-strk             # upgrade + swap
```

Options:
- `--slippage 0.02` sets swap slippage (default 0.01, max 0.1).
- `STARKNET_RPC=https://...` sets a custom RPC. Fallback endpoints live in `rpc_list.json`.

## Legacy accounts: the relayer flow

A legacy account can't pay for its own transactions. On the first run the tool creates a temporary relayer account, saves its key to `.env`, and asks you to send it at least 1 STRK. You can instead set `RELAYER_ACCOUNT`/`RELAYER_PRIVATE_KEY` to a wallet you already fund. On the next run the relayer pays for the account's signed upgrade, the account then pays its own gas, and the relayer's leftover STRK is sent back to your wallet.

## Example: recovering a legacy account

```
━━━ ACCOUNT ━━━
  Address:  0x01a2…9f3c
  Mode:     ✓ LIVE (will execute)

━━━ LEGACY ACCOUNT RECOVERY ━━━
  Argent/Ready proxy, implementation 0.2.2
  Path: → 0.2.3.1 (meta_tx_v0) → 0.4.0 (v3) → 0.5.0 + swaps
  ✓ Private key matches signer, no guardian

  Step 1/2: upgrade to 0.2.3.1 via meta_tx_v0
    This account can't send transactions itself, so a relayer account submits it (~0.05 STRK gas).
    Relayer: 0x07e4…b210 (1.5 STRK)
    Deploying relayer account
    Tx 0x0d01…a7e2 … ✓ confirmed
    Gas estimate: 0.0412 STRK
    Tx 0x0d02…41c9 … ✓ confirmed

  Step 2/2: upgrade to Argent 0.4.0 (account pays its own STRK gas)
    Gas estimate: 0.0301 STRK
    Tx 0x0d03…9b0f … ✓ confirmed
  Class:    0x0c1a…55e7

━━━ RELAYER REFUND ━━━
  Relayer 0x07e4…b210 holds 1.4197 STRK
  Returning 1.3735 STRK to 0x01a2…9f3c (gas up to 0.0462 STRK)
    Tx 0x0d04…ce31 … ✓ confirmed

━━━ UPGRADE (PRIORITY) ━━━
  Argent 0.4.0 -> 0.5.0
    Your balance:   3.4434 STRK
    Gas estimate:   0.0484 STRK
    Min required:   0.1452 STRK (gas + 2x buffer)
    Tx 0x0d05…7d88 … ✓ confirmed

━━━ HOLDINGS ━━━
  • 0.042 ETH
    → 1874.3 STRK ($96.12) via Ekubo 100%
    → Swap queued
  • 25.5 USDC
    → 497.06 STRK ($25.49) via Ekubo 60%, JediSwapCL 40%
    → Swap queued
  • 3.395 STRK

━━━ EXECUTION ━━━
    Your balance:   3.395 STRK
    Gas estimate:   0.3403 STRK
    Min required:   1.0209 STRK (gas + 2x buffer)
  → Swaps execute directly (gas from STRK balance)
    Tx 0x0d06…f514 … ✓ confirmed

━━━ COMPLETE ━━━
  ✓ Transactions executed
```

## Safety

- Keys stay on your machine. They are never printed, and `.env` is gitignored.
- AVNU swap calls are checked before signing: approve + swap only, payout to your account, min output within slippage.
- Mainnet only, use at your own risk. Run `--dry-run` first.

Details for AI agents and contributors: [`llms.txt`](llms.txt). Legacy recovery is a Rust port of [argentlabs/upgrade-v0-account](https://github.com/argentlabs/upgrade-v0-account).
