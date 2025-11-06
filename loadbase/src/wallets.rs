//! Wallet derivation + parallel ETH prefund with strictly monotonic nonces
//! (no gaps when some wallets are already funded).
//!
//! Up‑front rich‑balance check; iterator `.await` fixed (now uses explicit loop).

use anyhow::{anyhow, Result};
use ethers::{
    prelude::*,
    signers::{coins_bip39::English, LocalWallet, MnemonicBuilder},
    types::{TransactionRequest, U256},
    utils::format_units,
};
use futures_util::future::join_all;
use std::time::Duration;

/// Derive `n` wallets from the mnemonic (m/44'/60'/0'/0/i).
pub fn derive(mnemonic: &str, n: u32, chain_id: u64) -> Result<Vec<LocalWallet>> {
    let builder = MnemonicBuilder::<English>::default().phrase(mnemonic);
    (0..n)
        .map(|i| Ok(builder.clone().index(i)?.build()?.with_chain_id(chain_id)))
        .collect()
}

/// Prefund wallets concurrently while keeping consecutive nonces.
///
/// Performs an up‑front balance check on the rich account.
pub async fn prefund_varied<S: Signer + 'static>(
    rich: &SignerMiddleware<Provider<Http>, S>,
    dests: &[Address],
    amounts: &[U256],
) -> Result<()> {
    assert_eq!(dests.len(), amounts.len(), "length mismatch");

    let provider   = rich.provider();
    let gas_price  = provider.get_gas_price().await
        .unwrap_or_else(|_| U256::from(3_000_000_000u64)); // fallback 3 gwei
    let per_tx_gas = U256::from(50_000) * gas_price;

    // ---------- compute how many txs we need & total ETH ----------
    let mut total_needed = U256::zero();
    let mut tx_count     = 0u64;

    for (&addr, &target) in dests.iter().zip(amounts) {
        let bal = provider.get_balance(addr, None).await?;
        if bal < target {
            total_needed += target - bal;
            tx_count += 1;
        }
    }
    let overhead = per_tx_gas * U256::from(tx_count);

    // ---------- make sure rich account can cover it ---------------
    let rich_balance = provider.get_balance(rich.signer().address(), None).await?;

    if rich_balance < total_needed + overhead {
        return Err(anyhow!(
            "rich account {} balance {} ETH is insufficient.\n\
             Need ≈ {} ETH for transfers + {} ETH gas ({} tx * 21 000 * {} gwei)",
            rich.signer().address(),
            format_units(rich_balance, 18)?,
            format_units(total_needed, 18)?,
            format_units(overhead, 18)?,
            tx_count,
            gas_price / 1_000_000_000u64
        ));
    }

    // ---------- original prefund logic ----------------------------
    let mut next_nonce = provider
        .get_transaction_count(rich.signer().address(), Some(BlockNumber::Pending.into()))
        .await?;

    println!("▶ ETH prefund (parallel) …");
    let mut pendings = Vec::new();

    for (idx, (&addr, &target)) in dests.iter().zip(amounts).enumerate() {
        let bal_before = provider.get_balance(addr, None).await?;
        if bal_before >= target {
            println!(
                "   wallet #{:<4} {} ≥ target ({} ETH)",
                idx,
                addr,
                format_units(target, 18)?
            );
            continue; // no tx, nonce not consumed
        }

        let need = target - bal_before;
        let tx = TransactionRequest::pay(addr, need)
            .from(rich.signer().address())
            .gas(U256::from(210_000))
            .gas_price(gas_price)
            .nonce(next_nonce);

        println!(
            "   tx #{:<4} nonce {} → {addr:?} need {} wei  hash …",
            idx,
            next_nonce,
            need
        );

        let pending = rich.send_transaction(tx, None).await?;
        println!("      hash 0x{:x}", pending.tx_hash());

        pendings.push(pending);
        next_nonce += U256::one(); // advance only when we send
    }

    // wait receipts in parallel
    join_all(pendings.into_iter().map(|p| async move { let _ = p.await; })).await;
    println!("   all prefund txs mined, verifying …");

    // bounded verification (≤20 s) so we never hang
    const MAX_WAIT: Duration = Duration::from_secs(20);
    let start = tokio::time::Instant::now();
    loop {
        let mut still_low = Vec::new();
        for (idx, (&addr, &target)) in dests.iter().zip(amounts).enumerate() {
            if provider.get_balance(addr, None).await? < target {
                still_low.push((idx, addr));
            }
        }
        if still_low.is_empty() || start.elapsed() >= MAX_WAIT {
            if still_low.is_empty() {
                println!("   ✅ ETH prefund finished\n");
            } else {
                println!("   ⚠️ still under‑funded after 20 s:");
                for (idx, a) in still_low {
                    println!("      wallet #{idx} {a:?}");
                }
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}
