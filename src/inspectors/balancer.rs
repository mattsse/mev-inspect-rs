use crate::{
    addresses::BALANCER_PROXY,
    inspectors::find_matching,
    traits::Inspector,
    types::{actions::Trade, Classification, Inspection, Protocol},
    DefiProtocol, ProtocolContracts,
};

use crate::model::{CallClassification, InternalCall};
use ethers::{
    contract::{abigen, BaseContract},
    types::{Address, U256},
};

abigen!(BalancerPool, "abi/bpool.json");
abigen!(BalancerProxy, "abi/bproxy.json");

#[derive(Debug, Clone)]
/// An inspector for Uniswap
pub struct Balancer {
    bpool: BaseContract,
    bproxy: BaseContract,
}

impl Default for Balancer {
    /// Constructor
    fn default() -> Self {
        Self {
            bpool: BaseContract::from(BALANCERPOOL_ABI.clone()),
            bproxy: BaseContract::from(BALANCERPROXY_ABI.clone()),
        }
    }
}

type Swap = (Address, U256, Address, U256, U256);

impl DefiProtocol for Balancer {
    fn base_contracts(&self) -> ProtocolContracts {
        ProtocolContracts::Dual(&self.bpool, &self.bproxy)
    }

    fn protocol() -> Protocol {
        Protocol::Balancer
    }

    fn is_protocol(&self, to: &Address) -> Option<bool> {
        // TODO: Adjust for exchange proxy calls
        Some(*to == *BALANCER_PROXY)
    }

    fn classify_call(&self, call: &InternalCall) -> Option<CallClassification> {
        self.bpool
            .decode::<Swap, _>("swapExactAmountIn", &call.input)
            .or_else(|_| {
                self.bpool
                    .decode::<Swap, _>("swapExactAmountOut", &call.input)
            })
            .map(|_| CallClassification::Swap)
            .ok()
    }
}

impl Inspector for Balancer {
    fn inspect(&self, inspection: &mut Inspection) {
        let actions = inspection.actions.to_vec();
        let mut prune = Vec::new();
        for i in 0..inspection.actions.len() {
            let action = &mut inspection.actions[i];

            if let Some(calltrace) = action.as_call() {
                let call = calltrace.as_ref();
                let (token_in, _, token_out, _, _) = if let Ok(inner) = self
                    .bpool
                    .decode::<Swap, _>("swapExactAmountIn", &call.input)
                {
                    inner
                } else if let Ok(inner) = self
                    .bpool
                    .decode::<Swap, _>("swapExactAmountOut", &call.input)
                {
                    inner
                } else {
                    if self.is_protocol(&calltrace.call.to).unwrap_or_default() {
                        inspection.protocols.insert(Protocol::Balancer);
                    }
                    continue;
                };

                // In Balancer, the 2 subtraces of the `swap*` call are the transfers
                // In both cases, the in asset is being transferred _to_ the pair,
                // and the out asset is transferred _from_ the pair
                let t1 = find_matching(
                    actions.iter().enumerate().skip(i + 1),
                    |t| t.as_transfer(),
                    |t| t.token == token_in,
                    true,
                );

                let t2 = find_matching(
                    actions.iter().enumerate().skip(i + 1),
                    |t| t.as_transfer(),
                    |t| t.token == token_out,
                    true,
                );

                match (t1, t2) {
                    (Some((j, t1)), Some((k, t2))) => {
                        if t1.from != t2.to || t2.from != t1.to {
                            continue;
                        }

                        *action =
                            Classification::new(Trade::new(t1.clone(), t2.clone()), Vec::new());
                        prune.push(j);
                        prune.push(k);

                        inspection.protocols.insert(Protocol::Balancer);
                    }
                    _ => {}
                };
            }
        }

        prune
            .iter()
            .for_each(|p| inspection.actions[*p] = Classification::Prune);
        // TODO: Add checked calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use crate::{
        addresses::ADDRESSBOOK,
        inspectors::ERC20,
        reducers::{ArbitrageReducer, TradeReducer},
        types::Inspection,
        Inspector, Reducer,
    };

    struct MyInspector {
        erc20: ERC20,
        balancer: Balancer,
        trade: TradeReducer,
        arb: ArbitrageReducer,
    }

    impl MyInspector {
        fn inspect(&self, inspection: &mut Inspection) {
            self.erc20.inspect(inspection);
            self.balancer.inspect(inspection);
            self.trade.reduce(inspection);
            self.arb.reduce(inspection);
            inspection.prune();
        }

        fn new() -> Self {
            Self {
                erc20: ERC20::new(),
                balancer: Balancer::default(),
                trade: TradeReducer,
                arb: ArbitrageReducer::default(),
            }
        }
    }

    #[test]
    fn bot_trade() {
        let mut inspection = read_trace("balancer_trade.json");
        let bal = MyInspector::new();
        bal.inspect(&mut inspection);

        let known = inspection.known();

        assert_eq!(known.len(), 4);
        let t1 = known[0].as_ref().as_transfer().unwrap();
        assert_eq!(
            t1.amount,
            U256::from_dec_str("134194492674651541324").unwrap()
        );
        let trade = known[1].as_ref().as_trade().unwrap();
        assert_eq!(
            trade.t1.amount,
            U256::from_dec_str("7459963749616500736").unwrap()
        );
        let _t2 = known[2].as_ref().as_transfer().unwrap();
        let _t3 = known[3].as_ref().as_transfer().unwrap();
    }

    #[test]
    fn comp_collect_trade() {
        let mut inspection = read_trace("balancer_trade2.json");
        let bal = MyInspector::new();
        bal.inspect(&mut inspection);

        let known = inspection.known();

        assert_eq!(known.len(), 3);
        let trade = known[0].as_ref().as_trade().unwrap();
        assert_eq!(
            trade.t1.amount,
            U256::from_dec_str("1882725882636").unwrap()
        );
        assert_eq!(ADDRESSBOOK.get(&trade.t1.token).unwrap(), "cDAI",);
        assert_eq!(
            trade.t2.amount,
            U256::from_dec_str("2048034448010009909").unwrap()
        );
        assert_eq!(ADDRESSBOOK.get(&trade.t2.token).unwrap(), "COMP",);

        // 2 comp payouts
        let t1 = known[1].as_ref().as_transfer().unwrap();
        assert_eq!(ADDRESSBOOK.get(&t1.token).unwrap(), "COMP",);
        let t2 = known[2].as_ref().as_transfer().unwrap();
        assert_eq!(ADDRESSBOOK.get(&t2.token).unwrap(), "COMP",);
    }
}
