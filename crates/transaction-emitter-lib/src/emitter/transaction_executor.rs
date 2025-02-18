// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use super::RETRY_POLICY;
use anyhow::{Context, Result};
use aptos_logger::{debug, sample, sample::SampleRate, warn};
use aptos_rest_client::Client as RestClient;
use aptos_sdk::{
    move_types::account_address::AccountAddress, types::transaction::SignedTransaction,
};
use aptos_transaction_generator_lib::{CounterState, TransactionExecutor};
use async_trait::async_trait;
use futures::future::join_all;
use rand::{rngs::StdRng, seq::SliceRandom, thread_rng, Rng, SeedableRng};
use std::{
    sync::atomic::AtomicUsize,
    time::{Duration, Instant},
};

// Reliable/retrying transaction executor, used for initializing
pub struct RestApiTransactionExecutor {
    pub rest_clients: Vec<RestClient>,
    pub max_retries: usize,
    pub retry_after: Duration,
}

impl RestApiTransactionExecutor {
    fn random_rest_client(&self) -> &RestClient {
        let mut rng = thread_rng();
        self.rest_clients.choose(&mut rng).unwrap()
    }

    fn random_rest_client_from_rng<R>(&self, rng: &mut R) -> &RestClient
    where
        R: Rng + ?Sized,
    {
        self.rest_clients.choose(rng).unwrap()
    }

    async fn submit_check_and_retry(
        &self,
        txn: &SignedTransaction,
        counters: &CounterState,
        run_seed: u64,
    ) -> Result<()> {
        for i in 0..self.max_retries {
            sample!(
                SampleRate::Duration(Duration::from_secs(60)),
                debug!(
                    "Running reliable/retriable fetching, current state: {}",
                    counters.show_detailed()
                )
            );

            // All transactions from the same sender, need to be submitted to the same client
            // in the same retry round, so that they are not placed in parking lot.
            // Do so by selecting a client via seeded random selection.
            let seed = [
                i.to_le_bytes().to_vec(),
                run_seed.to_le_bytes().to_vec(),
                txn.sender().to_vec(),
            ]
            .concat();
            let mut seeded_rng = StdRng::from_seed(*aptos_crypto::HashValue::sha3_256_of(&seed));
            let rest_client = self.random_rest_client_from_rng(&mut seeded_rng);
            let mut failed_submit = false;
            let mut failed_wait = false;
            let result = submit_and_check(
                rest_client,
                txn,
                self.retry_after,
                &mut failed_submit,
                &mut failed_wait,
            )
            .await;

            if failed_submit {
                counters.submit_failures[i.min(counters.submit_failures.len() - 1)]
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if !counters.by_client.is_empty() {
                    counters
                        .by_client
                        .get(&rest_client.path_prefix_string())
                        .map(|(_, submit_failures, _)| {
                            submit_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        });
                }
            }
            if failed_wait {
                counters.wait_failures[i.min(counters.wait_failures.len() - 1)]
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if !counters.by_client.is_empty() {
                    counters
                        .by_client
                        .get(&rest_client.path_prefix_string())
                        .map(|(_, _, wait_failures)| {
                            wait_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        });
                }
            }

            if result.is_ok() {
                counters
                    .successes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if !counters.by_client.is_empty() {
                    counters
                        .by_client
                        .get(&rest_client.path_prefix_string())
                        .map(|(successes, _, _)| {
                            successes.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        });
                }
                return Ok(());
            };
        }

        // if submission timeouts, it might still get committed:
        self.random_rest_client()
            .wait_for_signed_transaction_bcs(txn)
            .await?;

        counters
            .successes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

async fn submit_and_check(
    rest_client: &RestClient,
    txn: &SignedTransaction,
    wait_duration: Duration,
    failed_submit: &mut bool,
    failed_wait: &mut bool,
) -> Result<()> {
    let start = Instant::now();
    if let Err(err) = rest_client.submit_bcs(txn).await {
        sample!(
            SampleRate::Duration(Duration::from_secs(60)),
            warn!(
                "[{}] Failed submitting transaction: {}",
                rest_client.path_prefix_string(),
                err,
            )
        );
        *failed_submit = true;
        // even if txn fails submitting, it might get committed, so wait to see if that is the case.
    }
    if let Err(err) = rest_client
        .wait_for_transaction_by_hash(
            txn.clone().committed_hash(),
            txn.expiration_timestamp_secs(),
            None,
            Some(wait_duration.saturating_sub(start.elapsed())),
        )
        .await
    {
        sample!(
            SampleRate::Duration(Duration::from_secs(60)),
            warn!(
                "[{}] Failed waiting on a transaction: {}",
                rest_client.path_prefix_string(),
                err,
            )
        );
        *failed_wait = true;
        Err(err)?;
    }
    Ok(())
}

#[async_trait]
impl TransactionExecutor for RestApiTransactionExecutor {
    async fn get_account_balance(&self, account_address: AccountAddress) -> Result<u64> {
        Ok(RETRY_POLICY
            .retry(move || {
                self.random_rest_client()
                    .get_account_balance(account_address)
            })
            .await?
            .into_inner()
            .get())
    }

    async fn query_sequence_number(&self, account_address: AccountAddress) -> Result<u64> {
        Ok(RETRY_POLICY
            .retry(move || self.random_rest_client().get_account_bcs(account_address))
            .await?
            .into_inner()
            .sequence_number())
    }

    async fn execute_transactions_with_counter(
        &self,
        txns: &[SignedTransaction],
        counters: &CounterState,
    ) -> Result<()> {
        let run_seed: u64 = thread_rng().gen();

        join_all(
            txns.iter()
                .map(|txn| self.submit_check_and_retry(txn, counters, run_seed)),
        )
        .await
        .into_iter()
        .collect::<Result<Vec<()>, anyhow::Error>>()
        .with_context(|| {
            format!(
                "Tried executing {} txns, request counters: {:?}",
                txns.len(),
                counters.show_detailed()
            )
        })?;

        Ok(())
    }

    fn create_counter_state(&self) -> CounterState {
        CounterState {
            submit_failures: std::iter::repeat_with(|| AtomicUsize::new(0))
                .take(self.max_retries)
                .collect(),
            wait_failures: std::iter::repeat_with(|| AtomicUsize::new(0))
                .take(self.max_retries)
                .collect(),
            successes: AtomicUsize::new(0),
            by_client: self
                .rest_clients
                .iter()
                .map(|client| {
                    (
                        client.path_prefix_string(),
                        (
                            AtomicUsize::new(0),
                            AtomicUsize::new(0),
                            AtomicUsize::new(0),
                        ),
                    )
                })
                .collect(),
        }
    }
}
