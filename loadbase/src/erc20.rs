// src/erc20.rs
//! ERC-20 deploy, mint, and sequential variable distribution.

use anyhow::Result;
use ethers::{
    contract::abigen,
    prelude::*,
    types::{Address, U256},
    utils::format_units,
};
use std::{sync::Arc, time::Duration};
use tokio::time::{sleep, timeout};

abigen!(
    SimpleERC20,
    "./contracts/out/SimpleERC20.sol/SimpleERC20.json"
);

pub use simple_erc20::SimpleERC20;

// ───────────── deploy + mint ─────────────
pub async fn deploy_and_mint<S: Signer + 'static>(
    signer: Arc<SignerMiddleware<Provider<Http>, S>>,
    name: &str,
    symbol: &str,
    supply: U256,
) -> Result<SimpleERC20<SignerMiddleware<Provider<Http>, S>>> {
    println!("▶ Deploying {name}/{symbol} …");
    let token = SimpleERC20::deploy(
        signer.clone(),
        (1_000_000_000u64, name.to_owned(), symbol.to_owned()),
    )?
    .confirmations(0usize)
    .send()
    .await?;
    println!("   deployed at {:?}\n", token.address());

    let call_mint = token.mint(signer.address(), supply);
    let pending_mint = call_mint.send().await?;
    println!("   mint tx hash 0x{:x}", pending_mint.tx_hash());
    pending_mint.await?;

    let supply_eth = format_units(token.total_supply().call().await?, 18)?;
    println!("   total supply {supply_eth}\n");
    Ok(token)
}

// ───────────── sequential distribution ─────────────
pub async fn distribute_varied<M: Middleware + 'static>(
    token: &SimpleERC20<M>,
    dests: &[Address],
    amounts: &[U256],
) -> Result<()> {
    assert_eq!(dests.len(), amounts.len(), "length mismatch");
    println!("▶ Distributing tokens sequentially …");

    let provider = token.client().clone();
    // CI runners may intermittently delay blocks; keep this generous to avoid false failures.
    let timeout_s = 180;
    const SEND_RETRIES: usize = 5;

    for (i, (&addr, &amt)) in dests.iter().zip(amounts).enumerate() {
        // 1. broadcast (retry when node reports replacement gas-price race)
        let mut send_attempt = 0usize;
        let tx_hash = loop {
            let send_result = {
                let call = token.transfer(addr, amt);
                call.send().await.map(|pending| pending.tx_hash())
            };
            match send_result {
                Ok(tx_hash) => break tx_hash,
                Err(err)
                    if is_replacement_gas_error(&err.to_string())
                        && send_attempt < SEND_RETRIES =>
                {
                    send_attempt += 1;
                    let backoff_ms = 300 * send_attempt as u64;
                    println!(
                        "      ⚠️ send attempt {} hit replacement gas-price race, retrying in {}ms: {}",
                        send_attempt, backoff_ms, err
                    );
                    sleep(Duration::from_millis(backoff_ms)).await;
                }
                Err(err) => return Err(err.into()),
            }
        };
        println!(
            "   tx #{:<3} → {addr:?}  amt {:>12}  hash 0x{tx_hash:x}",
            i,
            format_units(amt, 18)?
        );

        // 2. wait for receipt with timeout
        match timeout(Duration::from_secs(timeout_s), async {
            loop {
                match provider.get_transaction_receipt(tx_hash).await {
                    Ok(Some(rcpt)) => break Ok(rcpt),
                    Ok(None) => sleep(Duration::from_millis(150)).await,
                    Err(e) => break Err(anyhow::anyhow!(e)),
                }
            }
        })
        .await
        {
            Ok(Ok(rcpt)) if rcpt.status == Some(U64::one()) => {
                println!(
                    "      ✅ tx 0x{tx_hash:x} success (block {})",
                    rcpt.block_number.unwrap()
                );
            }
            Ok(Ok(rcpt)) => {
                println!(
                    "      ⚠️ tx 0x{tx_hash:x} reverted (status {:?})",
                    rcpt.status
                );
            }
            Ok(Err(e)) => {
                println!("      ⚠️ tx 0x{tx_hash:x} error {e}");
            }
            Err(_) => {
                println!("      ⏳ tx 0x{tx_hash:x} timed-out after {timeout_s}s");
            }
        }
    }

    println!("   ✅ distribution phase finished\n");
    Ok(())
}

fn is_replacement_gas_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("insufficient gas price to replace existing transaction")
        || lower.contains("replacement transaction underpriced")
}

#[cfg(test)]
mod tests {
    use super::is_replacement_gas_error;

    #[test]
    fn detects_replacement_underpriced_errors() {
        assert!(is_replacement_gas_error(
            "insufficient gas price to replace existing transaction"
        ));
        assert!(is_replacement_gas_error(
            "replacement transaction underpriced"
        ));
        assert!(!is_replacement_gas_error("nonce too low"));
    }
}
