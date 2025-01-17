use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::task::{Context, Poll};

use ethers::prelude::Middleware;
use ethers::types::{
    Block, BlockNumber, Filter, Log, Trace, Transaction, TransactionReceipt, TxHash, U256,
};
use futures::{
    stream::{self, FuturesUnordered},
    Stream, StreamExt, TryFutureExt,
};
use itertools::Itertools;
use thiserror::Error;

use crate::mevdb::BatchInserts;
use crate::model::EventLog;
use crate::types::{EvalError, Evaluation, TransactionData};
use crate::{DefiProtocol, HistoricalPrice, MevDB, TxReducer};
use std::convert::TryFrom;
use std::sync::Arc;

/// Classifies traces according to the provided inspectors
pub struct BatchInspector {
    inspectors: Vec<Box<dyn DefiProtocol + Send + Sync>>,
    reducers: Vec<Box<dyn TxReducer + Send + Sync>>,
}

impl BatchInspector {
    /// Constructor
    pub fn new(
        inspectors: Vec<Box<dyn DefiProtocol + Send + Sync>>,
        reducers: Vec<Box<dyn TxReducer + Send + Sync>>,
    ) -> Self {
        Self {
            inspectors,
            reducers,
        }
    }

    /// Decodes the inspection's actions
    pub fn inspect_tx(&self, tx: &mut TransactionData) {
        for inspector in self.inspectors.iter() {
            inspector.inspect_tx(tx);
        }
    }

    pub fn reduce_tx(&self, tx: &mut TransactionData) {
        for reducer in self.reducers.iter() {
            reducer.reduce_tx(tx);
        }
    }

    /// Evaluates all the blocks and evaluate them.
    ///
    /// This will return the `Evaluation`s of all the `Inspection`s for all the
    /// blocks in any order.
    ///
    /// No more than `max` evaluations will be buffered at
    /// any point in time.
    pub fn evaluate_blocks<M: Middleware + Unpin + 'static>(
        self: Arc<Self>,
        provider: Arc<M>,
        prices: Arc<HistoricalPrice<M>>,
        blocks: Range<u64>,
        max: usize,
    ) -> BatchEvaluator<M> {
        BatchEvaluator::new(self, provider, prices, blocks, max)
    }
}

/// Get the necessary information for processing a block
async fn get_block_info<M: Middleware + Unpin + 'static>(
    provider: Arc<M>,
    block_number: u64,
) -> Result<
    (
        Vec<Trace>,
        Block<Transaction>,
        Vec<TransactionReceipt>,
        Vec<Log>,
    ),
    BatchEvaluationError<M>,
> {
    let traces = provider
        .trace_block(BlockNumber::Number(block_number.into()))
        .map_err(|error| BatchEvaluationError::Block {
            block_number,
            error,
        });

    let block = provider
        .get_block_with_txs(block_number)
        .map_err(|error| BatchEvaluationError::Block {
            block_number,
            error,
        })
        .and_then(|block| {
            futures::future::ready(block.ok_or(BatchEvaluationError::NotFound(block_number)))
        });

    let receipts = provider
        .parity_block_receipts(block_number)
        .map_err(|error| BatchEvaluationError::Block {
            block_number,
            error,
        });

    let filter = Filter::new()
        .from_block(block_number)
        .to_block(block_number);
    // this should be fine for <10k logs in a block, at infura
    let logs = provider
        .get_logs(&filter)
        .map_err(|error| BatchEvaluationError::Block {
            block_number,
            error,
        });

    futures::try_join!(traces, block, receipts, logs)
}

type BlockStream<T> = Pin<
    Box<
        dyn Stream<
                Item = Result<
                    (
                        Vec<Trace>,
                        Block<Transaction>,
                        Vec<TransactionReceipt>,
                        Vec<Log>,
                    ),
                    BatchEvaluationError<T>,
                >,
            > + Send,
    >,
>;

type EvaluationResult<T> =
    Pin<Box<dyn Future<Output = Result<Evaluation, BatchEvaluationError<T>>> + Send>>;

pub struct BatchEvaluator<M: Middleware + 'static> {
    prices: Arc<HistoricalPrice<M>>,
    inspector: Arc<BatchInspector>,
    block_infos: BlockStream<M>,
    /// Evaluations that currently ongoing
    evaluations_queue: FuturesUnordered<EvaluationResult<M>>,
    /// `(TransactionData, gas_used, gas_price)` waiting to be evaluated
    waiting_inspections: VecDeque<(TransactionData, U256, U256)>,
    /// maximum allowed buffered futures
    max: usize,
    /// whether all block requests are done
    blocks_done: bool,
}

impl<M: Middleware + Unpin + 'static> BatchEvaluator<M> {
    fn new(
        inspector: Arc<BatchInspector>,
        provider: Arc<M>,
        prices: Arc<HistoricalPrice<M>>,
        blocks: Range<u64>,
        max: usize,
    ) -> Self {
        let block_infos = stream::iter(
            blocks
                .into_iter()
                .map(|block_number| get_block_info(Arc::clone(&provider), block_number))
                .collect::<Vec<_>>(),
        )
        .buffer_unordered(max);

        Self {
            prices,
            inspector,
            block_infos: Box::pin(block_infos),
            evaluations_queue: FuturesUnordered::new(),
            waiting_inspections: VecDeque::new(),
            max,
            blocks_done: false,
        }
    }

    /// Turn this stream into a `BatchInserter` that inserts all the `Evaluation`s
    pub fn insert_all<'a>(self, mev_db: MevDB) -> BatchInserts<'a, M> {
        BatchInserts::new(mev_db, self)
    }

    fn queue_in_evaluation(&mut self, tx: TransactionData, gas_used: U256, gas_price: U256) {
        let block_number = tx.block_number;
        let hash = tx.hash;
        let prices = Arc::clone(&self.prices);
        let eval = Box::pin(async move {
            Evaluation::new(tx, prices.as_ref(), gas_used, gas_price)
                .map_err(move |error| BatchEvaluationError::Evaluation {
                    block_number,
                    hash,
                    error,
                })
                .await
        });
        self.evaluations_queue.push(eval);
    }
}

impl<M: Middleware + Unpin + 'static> Stream for BatchEvaluator<M> {
    type Item = Result<Evaluation, BatchEvaluationError<M>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // queue in buffered evaluation jobs
        while this.evaluations_queue.len() < this.max {
            if let Some((inspection, gas_used, gas_price)) = this.waiting_inspections.pop_front() {
                this.queue_in_evaluation(inspection, gas_used, gas_price);
                log::trace!(
                    "queued new evaluation job, active: {}, waiting: {}",
                    this.evaluations_queue.len(),
                    this.waiting_inspections.len()
                );
            } else {
                break;
            }
        }

        while this.evaluations_queue.len() < this.max {
            match this.block_infos.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok((traces, block, receipts, logs)))) => {
                    log::trace!("fetched block infos for block {:?}", block.number);
                    let gas_price_txs = block
                        .transactions
                        .iter()
                        .map(|tx| (tx.hash, tx.gas_price))
                        .collect::<HashMap<TxHash, U256>>();

                    // tx -> logs
                    let mut all_tx_logs = logs
                        .into_iter()
                        .filter_map(|log| EventLog::try_from(log).ok())
                        .into_group_map_by(|log| log.transaction_hash);

                    let gas_used_txs = receipts
                        .into_iter()
                        .map(|receipt| {
                            (
                                receipt.transaction_hash,
                                receipt.gas_used.unwrap_or_default(),
                            )
                        })
                        .collect::<HashMap<TxHash, U256>>();

                    for mut tx in traces
                        .clone()
                        .into_iter()
                        .group_by(|t| t.transaction_hash.expect("tx hash exists"))
                        .into_iter()
                        .filter_map(|(tx, tx_traces)| {
                            let tx_logs = all_tx_logs.remove(&tx).unwrap_or_default();
                            TransactionData::create(tx_traces, tx_logs).ok()
                        })
                    {
                        this.inspector.inspect_tx(&mut tx);
                        this.inspector.reduce_tx(&mut tx);

                        let gas_used = gas_used_txs.get(&tx.hash).cloned().unwrap_or_default();

                        let gas_price = gas_price_txs.get(&tx.hash).cloned().unwrap_or_default();

                        if this.evaluations_queue.len() < this.max {
                            this.queue_in_evaluation(tx, gas_used, gas_price)
                        } else {
                            this.waiting_inspections
                                .push_back((tx, gas_used, gas_price));
                        }
                    }
                }
                Poll::Ready(Some(Err(err))) => {
                    return {
                        log::error!("failed to fetch block: {:?}", err);
                        Poll::Ready(Some(Err(err)))
                    }
                }
                Poll::Pending => break,
                Poll::Ready(None) => {
                    log::trace!("all blocks fetched");
                    this.blocks_done = true;
                    break;
                }
            }
        }

        // pull the next value from the evaluations_queue
        match this.evaluations_queue.poll_next_unpin(cx) {
            x @ Poll::Pending | x @ Poll::Ready(Some(_)) => {
                log::trace!("finished evaluation");
                return x;
            }
            Poll::Ready(None) => {}
        }

        // If more values are still coming from the stream, we're not done yet
        if this.blocks_done
            && this.evaluations_queue.is_empty()
            && this.waiting_inspections.is_empty()
        {
            log::info!("batch done");
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (blocks, _) = self.block_infos.size_hint();
        let evals = self.evaluations_queue.len();
        let waiting = self.waiting_inspections.len();
        (blocks + evals + waiting, None)
    }
}

#[derive(Debug, Error)]
pub enum BatchEvaluationError<M: Middleware + 'static> {
    #[error("Block {0} does not exist")]
    NotFound(u64),
    /// An evaluation of an inspection failed
    #[error(
        "Failed to evaluate inspection with tx hash {} of block {}: {:?}",
        block_number,
        hash,
        error
    )]
    Evaluation {
        /// The block number of the inspection
        block_number: u64,
        /// The trace's tx hash
        hash: TxHash,
        /// The reason why it failed
        error: EvalError<M>,
    },
    #[error("Failed to get block {}: {:?}", block_number, error)]
    Block {
        /// The block number of the inspection
        block_number: u64,
        /// The reason why it failed
        error: <M as Middleware>::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        addresses::{ADDRESSBOOK, WETH},
        inspectors::*,
        reducers::*,
        set,
        test_helpers::*,
        types::{Protocol, Status},
    };
    use ethers::types::U256;

    #[test]
    // call that starts from a bot but has a uniswap sub-trace
    // https://etherscan.io/tx/0x93690c02fc4d58734225d898ea4091df104040450c0f204b6bf6f6850ac4602f
    // 99k USDC -> 281 ETH -> 5.7 YFI trade
    // Liquidator Repay -> 5.7 YFI
    // Liquidation -> 292 ETH
    // Profit: 11 ETH
    fn aave_uni_liquidation() {
        let mut tx = get_tx("0x93690c02fc4d58734225d898ea4091df104040450c0f204b6bf6f6850ac4602f");

        let inspector = test_inspector();
        inspector.inspect_tx(&mut tx);
        inspector.reduce_tx(&mut tx);

        let liquidation = tx.actions().profitable_liquidations().next().unwrap();
        assert_eq!(
            liquidation.profit,
            U256::from_dec_str("11050220339336811520").unwrap()
        );
        assert_eq!(
            tx.protocols(),
            // SushiSwap is touched in a static call. The bot probably
            // checked whether it was more profitable to trade the
            // ETH for YFI on Sushi or Uni
            set![Protocol::UniswapV2, Protocol::Sushiswap, Protocol::Aave]
        );

        assert_eq!(ADDRESSBOOK.get(&liquidation.token).unwrap(), "ETH");
        assert_eq!(
            ADDRESSBOOK.get(&liquidation.as_ref().sent_token).unwrap(),
            "YFI"
        );
    }

    #[test]
    // https://etherscan.io/tx/0x46f4a4d409b44d85e64b1722b8b0f70e9713eb16d2c89da13cffd91486442627
    fn balancer_uni_arb_2() {
        let mut tx = get_tx("0x46f4a4d409b44d85e64b1722b8b0f70e9713eb16d2c89da13cffd91486442627");

        let inspector = test_inspector();
        inspector.inspect_tx(&mut tx);
        inspector.reduce_tx(&mut tx);

        let arb = tx.actions().arbitrage().next().unwrap();
        assert_eq!(arb.profit, U256::from_dec_str("41108016724856778").unwrap());
        assert_eq!(arb.token, *WETH);
        assert_eq!(
            tx.protocols(),
            set![Protocol::UniswapV2, Protocol::Balancer]
        );
    }

    #[test]
    // https://etherscan.io/tx/0x1d9a2c8bfcd9f6e133c490d892fe3869bada484160a81966e645616cfc21652a
    fn balancer_uni_arb2() {
        let mut tx = get_tx("0x1d9a2c8bfcd9f6e133c490d892fe3869bada484160a81966e645616cfc21652a");

        let inspector = test_inspector();
        inspector.inspect_tx(&mut tx);
        inspector.reduce_tx(&mut tx);

        let arb = tx.actions().arbitrage().next().unwrap();
        assert_eq!(arb.profit, U256::from_dec_str("47597234528640869").unwrap());
        assert_eq!(arb.token, *WETH);
        assert_eq!(
            tx.protocols(),
            set![Protocol::UniswapV2, Protocol::Balancer]
        );
    }

    #[test]
    fn curve_arb() {
        let mut tx = read_tx("curve_arb.data.json");

        let inspector = BatchInspector::new(
            vec![
                Box::new(Uniswap::default()),
                Box::new(Curve::new(vec![])),
                Box::new(ERC20::new()),
            ],
            vec![Box::new(TradeReducer), Box::new(ArbitrageReducer)],
        );
        inspector.inspect_tx(&mut tx);
        inspector.reduce_tx(&mut tx);

        let arb = tx.actions().arbitrage().next().unwrap();
        assert_eq!(arb.profit, U256::from_dec_str("14397525374450478").unwrap());
        assert_eq!(arb.token, *WETH);
        assert_eq!(
            tx.protocols(),
            set![Protocol::Sushiswap, Protocol::Curve, Protocol::ZeroEx]
        );
    }

    #[test]
    // https://etherscan.io/tx/0x1c85df1fa4c2e9fe7acc7bf204681aa0072b5df05e06bbc8e593777c0dfa5c1c
    fn bot_selfdestruct() {
        let mut tx = read_tx("bot_selfdestruct.data.json");

        let inspector = test_inspector();
        inspector.inspect_tx(&mut tx);
        inspector.reduce_tx(&mut tx);

        assert_eq!(tx.status, Status::Reverted);
        assert_eq!(
            tx.protocols(),
            set![Protocol::UniswapV2, Protocol::Balancer]
        )
    }

    #[test]
    // http://etherscan.io/tx/0x0e0e7c690589d9b94c3fbc4bae8abb4c5cac5c965abbb5bf1533e9f546b10b92
    fn dydx_aave_liquidation() {
        let mut tx = read_tx("dydx_loan.data.json");

        let inspector = test_inspector();
        inspector.inspect_tx(&mut tx);
        inspector.reduce_tx(&mut tx);

        assert_eq!(tx.status, Status::Success);
        assert_eq!(
            tx.protocols(),
            set![Protocol::Aave, Protocol::DyDx, Protocol::UniswapV2]
        );
        let liquidation = tx.actions().profitable_liquidations().next().unwrap();
        assert_eq!(
            liquidation.profit,
            U256::from_dec_str("18789801420638046861").unwrap()
        );
    }

    #[test]
    // http://etherscan.io/tx/0x97afae49a25201dbb34502d36a7903b51754362ceb231ff775c07db540f4a3d6
    // here the trader keeps the received asset (different than the one he used to repay)
    fn liquidation1() {
        let mut tx = read_tx("liquidation_1.data.json");

        let inspector = test_inspector();
        inspector.inspect_tx(&mut tx);
        inspector.reduce_tx(&mut tx);

        assert_eq!(tx.status, Status::Success);
        assert_eq!(tx.protocols(), set![Protocol::UniswapV2, Protocol::Aave]);
        let liquidation = tx.actions().liquidations().next().unwrap();
        assert_eq!(ADDRESSBOOK.get(&liquidation.sent_token).unwrap(), "BAT");
        assert_eq!(ADDRESSBOOK.get(&liquidation.received_token).unwrap(), "DAI");
    }

    // #[test]
    // // This was a failed attempt at a triangular arb between zHEGIC/WETH, zHEGIC/HEGIC
    // // and the HEGIC/WETH pools. The arb, if successful, would've yielded 0.1 ETH:
    // // 1. Known bot sends 115 WETH to 0xa084 (their proxy)
    // // 2. 0xa084 trades 3.583 WETH for zHEGIC
    // // 3. trades zHEGIC for HEGIC
    // // 4. trades HEGIC for 3.685 WETH whcih stays at 0xa084
    // // 5. send the remaining 111 WETH back to known bot
    // fn reverted_arb_positive_revenue() {
    //     let mut inspection = read_trace("reverted_arb.json");
    //
    //     let inspector = BatchInspector::new(
    //         vec![Box::new(ERC20::new()), Box::new(Uniswap::default())],
    //         vec![Box::new(TradeReducer), Box::new(ArbitrageReducer)],
    //     );
    //     inspector.inspect(&mut inspection);
    //     inspector.reduce(&mut inspection);
    //     inspection.prune();
    //
    //     let arb = inspection
    //         .known()
    //         .iter()
    //         .find_map(|x| x.as_ref().as_arbitrage())
    //         .cloned()
    //         .unwrap();
    //     assert_eq!(arb.profit.to_string(), "101664758086906735");
    //     assert_eq!(inspection.status, Status::Reverted);
    // }

    #[test]
    // This is added to ensure we do not misclassify Zapper txs
    // https://github.com/flashbots/mev-inspect-ts/issues/14
    fn zapper_no_false_positive() {
        let mut tx = read_tx("zapper1.data.json");

        let inspector = test_inspector();
        inspector.inspect_tx(&mut tx);
        inspector.reduce_tx(&mut tx);

        // first a trade gets classified
        let trade = tx.actions().trades().next().unwrap();
        assert_eq!(trade.t1.amount.to_string(), "1101651860618174754");
        assert_eq!(trade.t2.amount.to_string(), "3387662");

        dbg!(tx.protocols());
        dbg!(tx.actions().add_liquidity().collect::<Vec<_>>());
        // then the addliquidity call gets classified
        let add_liquidity = tx.actions().add_liquidity().next().unwrap();
        assert_eq!(
            add_liquidity.amounts,
            vec![
                U256::from_dec_str("3387662").unwrap(),
                U256::from_dec_str("1098347873288497855").unwrap(),
            ]
        );
    }
}
