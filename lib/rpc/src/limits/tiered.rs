use super::Limits;
use std::num::NonZeroU32;

const fn nz(rps: u32) -> NonZeroU32 {
    NonZeroU32::new(rps).expect("rps must be non-zero")
}

/// What one node should sustain.
const TOTAL: NonZeroU32 = nz(1000);

/// M bucket — bounded multi-step ops (single EVM exec, single proof, O(block tx count)).
const M: NonZeroU32 = nz(200);

/// M-bucket methods.
const M_METHODS: &[&str] = &[
    "eth_getBlockReceipts",
    "eth_fillTransaction",
    "eth_call",
    "eth_estimateGas",
    "zks_getProof",
    "ots_getBlockTransactions",
    "txpool_inspect",
];

/// Methods with their own per-method RPS, distinct from the M bucket.
const CUSTOM_METHODS: &[(&str, NonZeroU32)] = &[
    ("eth_getLogs", nz(200)),
    ("eth_simulateV1", nz(1)),
    ("debug_traceTransaction", nz(10)),
    ("debug_traceCall", nz(10)),
    ("debug_traceBlockByHash", nz(10)),
    ("debug_traceBlockByNumber", nz(10)),
    ("zks_getL2ToL1LogProof", nz(10)),
    ("ots_searchTransactionsBefore", nz(10)),
    ("ots_searchTransactionsAfter", nz(10)),
    ("txpool_content", nz(10)),
];

impl Limits {
    /// Hardcoded per-node RPS limits grouped into three buckets:
    /// - **S** (O(1) reads) — no per-method limit; only counted against the global cap.
    /// - **M** ([`M_METHODS`]) — [`M`] RPS each.
    /// - **Custom** ([`CUSTOM_METHODS`]) — explicit RPS per method.
    ///
    /// Scale capacity by running more nodes behind a load balancer.
    pub(crate) fn tiered() -> Self {
        let m = M_METHODS.iter().map(|&name| (name.to_string(), M));
        let custom = CUSTOM_METHODS
            .iter()
            .map(|&(name, rps)| (name.to_string(), rps));
        Self {
            global_rps: Some(TOTAL),
            methods: m.chain(custom).collect(),
        }
    }
}
