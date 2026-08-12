use alloy::primitives::U256;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::Block as EthBlock;
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::SealedBlock;
use reth_storage_api::{AccountInfoReader, StateProviderFactory};
use reth_transaction_pool::error::InvalidPoolTransactionError;
use reth_transaction_pool::{
    EthPoolTransaction, EthTransactionValidator, TransactionOrigin, TransactionValidationOutcome,
    TransactionValidator,
};
use std::sync::RwLock;
use zk_os_api::helpers::validate_l2_tx_intrinsic_native_resources;
use zksync_os_types::{FeeParams, ProtocolSemanticVersion};

/// A wrapper around [`EthTransactionValidator`] that adds ZKSync OS specific
/// stateless validation on top of the standard Ethereum checks.
///
/// The extra L2 checks rely only on the transaction and the latest fee params /
/// protocol version cached on `self`, so they don't need access to on-chain state and
/// run during the stateless phase.
///
/// The validation pipeline mirrors reth's own call chain:
///
/// ```text
/// TransactionValidator::validate_transaction
///   └─ validate_one
///        └─ validate_one_with_provider
///             ├─ self.validate_stateless   (our override, then calls inner.validate_stateless)
///             └─ inner.validate_stateful   (delegated to EthTransactionValidator)
/// ```
#[derive(Debug)]
pub(crate) struct ZkTransactionValidator<Client, Tx> {
    inner: EthTransactionValidator<Client, Tx, EthEvmConfig>,
    fee_params: RwLock<FeeParams>,
    /// Protocol version expected for the next produced block. Drives version-gated
    /// stateless checks (e.g. intrinsic native resources, available from v31).
    /// Starts as `None` (version-gated checks disabled) and is populated on canonical state
    /// changes — at least one block is replayed before block production starts.
    protocol_version: RwLock<Option<ProtocolSemanticVersion>>,
}

impl<Client, Tx> ZkTransactionValidator<Client, Tx> {
    pub(crate) fn new(inner: EthTransactionValidator<Client, Tx, EthEvmConfig>) -> Self {
        // Before the first `update_fee_params` call, treat the chain as a 0 gas price chain with
        // unlimited native resource: basefee/pubdata are 0, native_price is 1 (not 0) so that any
        // divisions by native_price remain well-defined.
        let fee_params = FeeParams {
            eip1559_basefee: U256::ZERO,
            native_price: U256::from(1u64),
            pubdata_price: U256::ZERO,
        };
        Self {
            inner,
            fee_params: RwLock::new(fee_params),
            protocol_version: RwLock::new(None),
        }
    }

    pub(crate) fn update_fee_params(&self, fee_params: FeeParams) {
        *self.fee_params.write().expect("lock poisoned") = fee_params;
    }

    pub(crate) fn update_protocol_version(&self, protocol_version: ProtocolSemanticVersion) {
        *self.protocol_version.write().expect("lock poisoned") = Some(protocol_version);
    }
}

impl<Client, Tx> ZkTransactionValidator<Client, Tx>
where
    Client: ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks> + StateProviderFactory,
    Tx: EthPoolTransaction,
{
    /// Checks that the transaction's `gas_limit` and `gas_price` covers the intrinsic native
    /// resources (computational + pubdata) cost under the currently cached fee params.
    fn validate_intrinsic_native_resources(
        &self,
        transaction: &Tx,
    ) -> Result<(), InvalidPoolTransactionError> {
        let fee_params = *self.fee_params.read().expect("lock poisoned");
        let (access_list_accounts, access_list_storage_keys) = transaction
            .access_list()
            .map(|l| {
                (
                    l.len() as u64,
                    l.iter().map(|i| i.storage_keys.len()).sum::<usize>() as u64,
                )
            })
            .unwrap_or((0, 0));
        let authorization_list_num = transaction
            .authorization_list()
            .map(|l| l.len())
            .unwrap_or(0) as u64;
        // if base_fee > max_fee_per_gas, gas_price and correspondingly native limit can't be calculated
        // We can try to calculate native limit using some estimated inclusion params, but for now
        // just skipping native validation. It's uncommon case.
        if fee_params.eip1559_basefee > U256::from(transaction.max_fee_per_gas()) {
            return Ok(());
        }
        validate_l2_tx_intrinsic_native_resources(
            fee_params.eip1559_basefee,
            fee_params.native_price,
            fee_params.pubdata_price,
            transaction.gas_limit(),
            transaction.input().len() as u64,
            access_list_accounts,
            access_list_storage_keys,
            authorization_list_num,
            U256::from(transaction.max_fee_per_gas()),
            transaction
                .max_priority_fee_per_gas()
                .map(U256::from)
                .unwrap_or(U256::ZERO),
        )
        .map_err(|()| InvalidPoolTransactionError::IntrinsicGasTooLow) // we return it as intrinsic gas error to user
    }

    /// Stateless validation with additional L2-specific checks.
    ///
    /// Runs custom checks first (using the latest fee params cached on `self`), then
    /// delegates to the inner [`EthTransactionValidator::validate_stateless`].
    fn validate_stateless(
        &self,
        origin: TransactionOrigin,
        transaction: &Tx,
    ) -> Result<(), InvalidPoolTransactionError> {
        self.inner.validate_stateless(origin, transaction)?;
        if self
            .protocol_version
            .read()
            .expect("lock poisoned")
            .as_ref()
            .is_some_and(ProtocolSemanticVersion::is_post_v31)
        {
            self.validate_intrinsic_native_resources(transaction)
        } else {
            Ok(())
        }
    }

    /// Validates a single transaction using an optional cached state provider.
    ///
    /// Mirrors [`EthTransactionValidator::validate_one_with_provider`] but routes
    /// stateless validation through [`Self::validate_stateless`].
    fn validate_one_with_provider(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        maybe_state: &mut Option<Box<dyn AccountInfoReader + Send>>,
    ) -> TransactionValidationOutcome<Tx> {
        if let Err(err) = self.validate_stateless(origin, &transaction) {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }

        if maybe_state.is_none() {
            match self.inner.client().latest() {
                Ok(new_state) => {
                    *maybe_state = Some(Box::new(new_state));
                }
                Err(err) => {
                    return TransactionValidationOutcome::Error(*transaction.hash(), Box::new(err));
                }
            }
        }

        let state = maybe_state.as_deref().expect("provider is set");
        self.inner.validate_stateful(origin, transaction, state)
    }

    pub(crate) fn validate_one(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
    ) -> TransactionValidationOutcome<Tx> {
        self.validate_one_with_provider(origin, transaction, &mut None)
    }

    fn validate_batch(
        &self,
        transactions: impl IntoIterator<Item = (TransactionOrigin, Tx)>,
    ) -> Vec<TransactionValidationOutcome<Tx>> {
        let mut provider = None;
        transactions
            .into_iter()
            .map(|(origin, tx)| self.validate_one_with_provider(origin, tx, &mut provider))
            .collect()
    }

    fn validate_batch_with_origin(
        &self,
        origin: TransactionOrigin,
        transactions: impl IntoIterator<Item = Tx> + Send,
    ) -> Vec<TransactionValidationOutcome<Tx>> {
        let mut provider = None;
        transactions
            .into_iter()
            .map(|tx| self.validate_one_with_provider(origin, tx, &mut provider))
            .collect()
    }
}

impl<Client, Tx> TransactionValidator for ZkTransactionValidator<Client, Tx>
where
    Client: ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks> + StateProviderFactory,
    Tx: EthPoolTransaction,
{
    type Transaction = Tx;
    type Block = EthBlock;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        self.validate_one(origin, transaction)
    }

    async fn validate_transactions(
        &self,
        transactions: impl IntoIterator<Item = (TransactionOrigin, Self::Transaction), IntoIter: Send>
        + Send,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        self.validate_batch(transactions)
    }

    async fn validate_transactions_with_origin(
        &self,
        origin: TransactionOrigin,
        transactions: impl IntoIterator<Item = Self::Transaction, IntoIter: Send> + Send,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        self.validate_batch_with_origin(origin, transactions)
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        TransactionValidator::on_new_head_block(&self.inner, new_tip_block)
    }
}
