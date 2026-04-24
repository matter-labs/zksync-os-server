use std::sync::RwLock;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives::Block as EthBlock;
use reth_primitives_traits::SealedBlock;
use reth_storage_api::{AccountInfoReader, StateProviderFactory};
use reth_transaction_pool::{
    EthPoolTransaction, EthTransactionValidator, TransactionOrigin,
    TransactionValidationOutcome, TransactionValidator,
};

/// A wrapper around [`EthTransactionValidator`] that adds ZKSync OS specific
/// stateful validation on top of the standard Ethereum checks.
///
/// The validation pipeline mirrors reth's own call chain:
///
/// ```text
/// TransactionValidator::validate_transaction
///   └─ validate_one
///        └─ validate_one_with_provider
///             ├─ inner.validate_stateless  (delegated to EthTransactionValidator)
///             └─ self.validate_stateful    (our override, then calls inner.validate_stateful)
/// ```
#[derive(Debug)]
pub(crate) struct ZkTransactionValidator<Client, Tx> {
    inner: EthTransactionValidator<Client, Tx, EthEvmConfig>,
}

impl<Client, Tx> ZkTransactionValidator<Client, Tx> {
    pub(crate) fn new(inner: EthTransactionValidator<Client, Tx, EthEvmConfig>) -> Self {
        Self { inner }
    }
}

impl<Client, Tx> ZkTransactionValidator<Client, Tx>
where
    Client: ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks> + StateProviderFactory,
    Tx: EthPoolTransaction,
{
    /// Stateful validation with additional L2-specific checks.
    ///
    /// Called after stateless validation passes. Runs custom L2 checks first,
    /// then delegates to the inner [`EthTransactionValidator::validate_stateful`].
    fn validate_stateful(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: impl AccountInfoReader,
    ) -> TransactionValidationOutcome<Tx> {
        // TODO: Add custom L2 validation checks here.
        // Example:
        // if let Err(err) = self.validate_l2_specific(&transaction, &state) {
        //     return TransactionValidationOutcome::Invalid(transaction, err);
        // }
        self.inner.validate_stateful(origin, transaction, state)
    }

    /// Validates a single transaction using an optional cached state provider.
    ///
    /// Mirrors [`EthTransactionValidator::validate_one_with_provider`] but routes
    /// stateful validation through [`Self::validate_stateful`].
    fn validate_one_with_provider(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        maybe_state: &mut Option<Box<dyn AccountInfoReader + Send>>,
    ) -> TransactionValidationOutcome<Tx> {
        match self.inner.validate_stateless(origin, transaction) {
            Ok(transaction) => {
                if maybe_state.is_none() {
                    match self.inner.client().latest() {
                        Ok(new_state) => {
                            *maybe_state = Some(Box::new(new_state));
                        }
                        Err(err) => {
                            return TransactionValidationOutcome::Error(
                                *transaction.hash(),
                                Box::new(err),
                            )
                        }
                    }
                }

                let state = maybe_state.as_deref().expect("provider is set");
                self.validate_stateful(origin, transaction, state)
            }
            Err(invalid_outcome) => invalid_outcome,
        }
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
        transactions: impl IntoIterator<
                Item = (TransactionOrigin, Self::Transaction),
                IntoIter: Send,
            > + Send,
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
