use alloy::eips::BlockId;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::U256;
use alloy::providers::ProviderBuilder;
use alloy::providers::ext::DebugApi;
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::trace::geth::{GethDebugTracerType, GethDebugTracingCallOptions, GethTrace};
use alloy::signers::local::LocalSigner;
use serde_json::Value;
use std::env;
use std::str::FromStr;
use tracing::info;
use zksync_os_integration_tests::contracts::{TracingPrimary, TracingSecondary};
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;

const ZKSYNC_URL_ENV: &str = "JS_TRACER_ZKSYNC_RPC_URL";
const RETH_URL_ENV: &str = "JS_TRACER_RETH_RPC_URL";
const PRIVATE_KEY_ENV: &str = "JS_TRACER_PRIVATE_KEY";

// This test is left here in case we want to do cross-node JS tracer comparisons in the future.
// It is ignored by default since it requires setting up two nodes and providing their RPC URLs
#[ignore]
#[test_log::test(tokio::test)]
async fn compare_js_tracer_outputs_between_nodes() -> anyhow::Result<()> {
    let Some(zksync_url) = env::var(ZKSYNC_URL_ENV).ok() else {
        info!("skipping cross-node tracer comparison; {ZKSYNC_URL_ENV} is not set");
        return Ok(());
    };
    let Some(reth_url) = env::var(RETH_URL_ENV).ok() else {
        info!("skipping cross-node tracer comparison; {RETH_URL_ENV} is not set");
        return Ok(());
    };
    let Some(private_key) = env::var(PRIVATE_KEY_ENV).ok() else {
        info!("skipping cross-node tracer comparison; {PRIVATE_KEY_ENV} is not set");
        return Ok(());
    };

    let wallet = EthereumWallet::new(LocalSigner::from_str(&private_key)?);
    let wallet_address = wallet.default_signer().address();

    let zksync_provider = ProviderBuilder::new()
        .wallet(wallet.clone())
        .connect(&zksync_url)
        .await?;
    let reth_provider = ProviderBuilder::new()
        .wallet(wallet.clone())
        .connect(&reth_url)
        .await?;

    let zksync_provider = EthDynProvider::new(zksync_provider);
    let reth_provider = EthDynProvider::new(reth_provider);

    // Deploy helper contracts on both nodes.
    let secondary_init_value = U256::from(7);
    let secondary_zksync =
        TracingSecondary::deploy(zksync_provider.clone(), secondary_init_value).await?;
    let primary_zksync =
        TracingPrimary::deploy(zksync_provider.clone(), *secondary_zksync.address()).await?;

    let secondary_reth =
        TracingSecondary::deploy(reth_provider.clone(), secondary_init_value).await?;
    let primary_reth =
        TracingPrimary::deploy(reth_provider.clone(), *secondary_reth.address()).await?;

    // Prepare calls we want to compare.
    let calculate_value = U256::from(3);
    let mut calc_zksync_request = primary_zksync
        .calculate(calculate_value)
        .into_transaction_request();
    configure_call_request(&mut calc_zksync_request, wallet_address);
    let mut calc_reth_request = primary_reth
        .calculate(calculate_value)
        .into_transaction_request();
    configure_call_request(&mut calc_reth_request, wallet_address);

    let call_scenarios = vec![("calculate", calc_zksync_request, calc_reth_request)];

    let tracers = vec![
        (
            "minimal_tracer",
            r#"
                {
                  maxSteps: 256,

                  setup: function () {
                    this.steps = [];
                  },

                  step: function (log, db) {
                    const rec = {
                      pc: log.getPC(),
                      op: log.op.toString(),
                      gas: log.getGas(),
                      gasCost: log.getCost(),
                      depth: log.getDepth(),
                      error: log.getError ? ("" + log.getError()) : null
                    };
                    this.steps.push(rec);
                    if (this.steps.length > this.maxSteps) this.steps.shift();
                  },

                  fault: function (log, db) {
                    this.faultLog = {
                      pc: log.getPC(),
                      op: log.op.toString(),
                      gas: log.getGas(),
                      depth: log.getDepth(),
                      error: "" + log.getError()
                    };
                  },

                  result: function (ctx, db) {
                    return {
                      type: "minimal-steps",
                      lastSteps: this.steps,
                      fault: this.faultLog || null
                    };
                  }
                }
            "#,
        ),
        (
            "value_transfer_tracer",
            r#"  {
                  setup: function () {
                    this.totalCost = 0;
                    this.byOp = {};
                  },

                  step: function (log, db) {
                    var op = log.op.toString();
                    var cost = log.getCost();
                    this.totalCost += cost;

                    var e = this.byOp[op];
                    if (!e) this.byOp[op] = { count: 1, cost: cost };
                    else { e.count += 1; e.cost += cost; }
                  },

                  result: function () {
                    var hot = [];
                    for (var k in this.byOp) {
                      hot.push({ op: k, count: this.byOp[k].count, cost: this.byOp[k].cost });
                    }
                    hot.sort(function (a, b) { return b.cost - a.cost; });

                    return {
                      type: "gas-profiler",
                      totalIntrinsicCostApprox: this.totalCost,
                      hotOpcodes: hot
                    };
                  }
                }
            "#,
        ),
        (
            "opcode_coverage_tracer",
            r#"
            {
              setup: function () {
                this.cover = {};
                this.pcs = {};
              },

              step: function (log, db) {
                var op = log.op.toString();
                this.cover[op] = true;

                var pc = log.getPC();
                var m = this.pcs[op];
                if (!m) { m = {}; this.pcs[op] = m; }
                m[pc] = true;
              },

              result: function () {
                var ops = [];
                for (var k in this.cover) {
                  var pcs = Object.keys(this.pcs[k]).map(function (x) { return Number(x); });
                  ops.push({ op: k, uniquePCs: pcs.length });
                }
                ops.sort(function (a, b) { return a.op < b.op ? -1 : 1; });

                return { type: "opcode-coverage", ops: ops };
              }
            }
            "#,
        ),
    ];

    for (scenario_name, zk_request, reth_request) in call_scenarios {
        for (tracer_name, tracer_code) in &tracers {
            let reth_trace =
                trace_with_js(&reth_provider, reth_request.clone(), tracer_code).await?;
            let zk_trace = trace_with_js(&zksync_provider, zk_request.clone(), tracer_code).await?;
            let mut zk_trace = zk_trace;
            let mut reth_trace = reth_trace;
            normalize_hex_strings(&mut zk_trace);
            normalize_hex_strings(&mut reth_trace);
            assert_eq!(
                zk_trace, reth_trace,
                "JS tracer '{tracer_name}' produced different output for scenario '{scenario_name}'"
            );
        }
    }

    Ok(())
}

fn configure_call_request(req: &mut TransactionRequest, from: alloy::primitives::Address) {
    req.set_from(from);
    req.max_priority_fee_per_gas = Some(1);
    req.max_fee_per_gas = Some(u128::MAX);
}

async fn trace_with_js(
    provider: &EthDynProvider,
    request: TransactionRequest,
    tracer_code: &str,
) -> anyhow::Result<Value> {
    let mut opts = GethDebugTracingCallOptions::default();

    opts.tracing_options.tracer = Some(GethDebugTracerType::JsTracer(tracer_code.to_string()));
    let trace = provider
        .debug_trace_call(request, BlockId::latest(), opts)
        .await?;
    match trace {
        GethTrace::JS(value) => Ok(value),
        other => anyhow::bail!("expected JS trace result, got {other:?}"),
    }
}

fn normalize_hex_strings(value: &mut Value) {
    match value {
        Value::String(data) => {
            if data.starts_with("0x") {
                *data = data.to_lowercase();
            }
        }
        Value::Array(values) => values.iter_mut().for_each(normalize_hex_strings),
        Value::Object(map) => map.values_mut().for_each(normalize_hex_strings),
        _ => {}
    }
}
