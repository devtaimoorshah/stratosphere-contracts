use std::cmp::Ordering;

use cosmwasm_std::{
    coin, ensure, Coin, Decimal256, Deps, Fraction, Order, StdResult, Uint128, Uint256,
};
use cw_storage_plus::Bound;
use mantra_dex_std::coin::aggregate_coins;
use mantra_dex_std::pool_manager::{
    AssetDecimalsResponse, Config, PoolInfoResponse, PoolType, PoolsResponse,
    ReverseSimulateSwapOperationsResponse, ReverseSimulationResponse,
    SimulateSwapOperationsResponse, SimulationResponse, SwapOperation,
};

use crate::helpers::get_asset_indexes_in_pool;
use crate::math::Decimal256Helper;
use crate::state::{CONFIG, POOLS};
use crate::{
    helpers::{self, calculate_stableswap_y, StableSwapDirection},
    state::get_pool_by_identifier,
    ContractError,
};

/// Query the config of the contract.
pub fn query_config(deps: Deps) -> Result<Config, ContractError> {
    Ok(CONFIG.load(deps.storage)?)
}

/// Query the native asset decimals
pub fn query_asset_decimals(
    deps: Deps,
    pool_identifier: String,
    denom: String,
) -> Result<AssetDecimalsResponse, ContractError> {
    let pool_info = get_pool_by_identifier(&deps, &pool_identifier)?;
    let decimal_index = pool_info
        .asset_denoms
        .iter()
        .position(|d| d.clone() == denom)
        .ok_or(ContractError::AssetMismatch)?;

    Ok(AssetDecimalsResponse {
        pool_identifier,
        denom,
        decimals: pool_info.asset_decimals[decimal_index],
    })
}

// Simulate a swap with the provided asset to determine the amount of the other asset that would be received
pub fn query_simulation(
    deps: Deps,
    offer_asset: Coin,
    ask_asset_denom: String,
    pool_identifier: String,
) -> Result<SimulationResponse, ContractError> {
    let pool_info = get_pool_by_identifier(&deps, &pool_identifier)?;

    let (offer_asset_in_pool, ask_asset_in_pool, _, _, offer_decimal, ask_decimal) =
        get_asset_indexes_in_pool(&pool_info, offer_asset.denom, ask_asset_denom)?;

    let swap_computation = helpers::compute_swap(
        Uint256::from(pool_info.assets.len() as u128),
        offer_asset_in_pool.amount,
        ask_asset_in_pool.amount,
        offer_asset.amount,
        pool_info.pool_fees,
        &pool_info.pool_type,
        offer_decimal,
        ask_decimal,
    )?;

    Ok(SimulationResponse {
        return_amount: swap_computation.return_amount,
        spread_amount: swap_computation.spread_amount,
        swap_fee_amount: swap_computation.swap_fee_amount,
        protocol_fee_amount: swap_computation.protocol_fee_amount,
        burn_fee_amount: swap_computation.burn_fee_amount,
        extra_fees_amount: swap_computation.extra_fees_amount,
    })
}

/// Queries a swap reverse simulation. Used to derive the number of source tokens returned for
/// the number of target tokens.
pub fn query_reverse_simulation(
    deps: Deps,
    ask_asset: Coin,
    offer_asset_denom: String,
    pool_identifier: String,
) -> Result<ReverseSimulationResponse, ContractError> {
    let pool_info = get_pool_by_identifier(&deps, &pool_identifier)?;

    let (offer_asset_in_pool, ask_asset_in_pool, _, _, offer_decimal, ask_decimal) =
        get_asset_indexes_in_pool(&pool_info, offer_asset_denom, ask_asset.denom)?;

    let pool_fees = pool_info.pool_fees;

    match pool_info.pool_type {
        PoolType::ConstantProduct => {
            let offer_amount_computation = helpers::compute_offer_amount(
                offer_asset_in_pool.amount,
                ask_asset_in_pool.amount,
                ask_asset.amount,
                pool_fees,
            )?;

            Ok(ReverseSimulationResponse {
                offer_amount: offer_amount_computation.offer_amount,
                spread_amount: offer_amount_computation.spread_amount,
                swap_fee_amount: offer_amount_computation.swap_fee_amount,
                protocol_fee_amount: offer_amount_computation.protocol_fee_amount,
                burn_fee_amount: offer_amount_computation.burn_fee_amount,
                extra_fees_amount: offer_amount_computation.extra_fees_amount,
            })
        }
        PoolType::StableSwap { amp } => {
            let offer_pool =
                Decimal256::decimal_with_precision(offer_asset_in_pool.amount, offer_decimal)?;
            let ask_pool =
                Decimal256::decimal_with_precision(ask_asset_in_pool.amount, ask_decimal)?;

            let mut extra_fees = Decimal256::zero();
            for extra_fee in pool_fees.extra_fees.iter() {
                extra_fees = extra_fees.checked_add(extra_fee.to_decimal_256())?;
            }

            let before_fees = (Decimal256::one()
                .checked_sub(pool_fees.protocol_fee.to_decimal_256())?
                .checked_sub(pool_fees.swap_fee.to_decimal_256())?
                .checked_sub(pool_fees.burn_fee.to_decimal_256())?)
            .checked_sub(extra_fees)?
            .inv()
            .unwrap_or_else(Decimal256::one)
            .checked_mul(Decimal256::decimal_with_precision(
                ask_asset.amount,
                ask_decimal,
            )?)?;

            let before_fees_offer = before_fees.to_uint256_with_precision(offer_decimal.into())?;
            let before_fees_ask = before_fees.to_uint256_with_precision(ask_decimal.into())?;

            let max_precision = offer_decimal.max(ask_decimal);

            let new_offer_pool_amount = calculate_stableswap_y(
                Uint256::from(pool_info.assets.len() as u128),
                offer_pool,
                ask_pool,
                before_fees,
                &amp,
                max_precision,
                StableSwapDirection::ReverseSimulate,
            )?;

            let offer_amount = new_offer_pool_amount.checked_sub(Uint128::try_from(
                offer_pool.to_uint256_with_precision(u32::from(max_precision))?,
            )?)?;

            // convert into the original offer precision
            let offer_amount = match max_precision.cmp(&offer_decimal) {
                Ordering::Equal => offer_amount,
                // note that Less should never happen (as max_precision = max(offer_decimal, ask_decimal))
                Ordering::Less => offer_amount.checked_mul(Uint128::new(
                    10u128.pow((offer_decimal - max_precision).into()),
                ))?,
                Ordering::Greater => offer_amount.checked_div(Uint128::new(
                    10u128.pow((max_precision - offer_decimal).into()),
                ))?,
            };

            let spread_amount = offer_amount.saturating_sub(Uint128::try_from(before_fees_offer)?);
            let swap_fee_amount = pool_fees.swap_fee.compute(before_fees_ask)?;
            let protocol_fee_amount = pool_fees.protocol_fee.compute(before_fees_ask)?;
            let burn_fee_amount = pool_fees.burn_fee.compute(before_fees_ask)?;

            let mut extra_fees_amount: Uint256 = Uint256::zero();
            for extra_fee in pool_fees.extra_fees.iter() {
                extra_fees_amount =
                    extra_fees_amount.checked_add(extra_fee.compute(before_fees_ask)?)?;
            }

            Ok(ReverseSimulationResponse {
                offer_amount,
                spread_amount,
                swap_fee_amount: swap_fee_amount.try_into()?,
                protocol_fee_amount: protocol_fee_amount.try_into()?,
                burn_fee_amount: burn_fee_amount.try_into()?,
                extra_fees_amount: extra_fees_amount.try_into()?,
            })
        }
    }
}

// settings for pagination
pub(crate) const MAX_LIMIT: u32 = 100;
const DEFAULT_LIMIT: u32 = 10;

/// Gets the pools in the contract. Returns a [PoolsResponse].
pub fn get_pools(
    deps: Deps,
    pool_identifier: Option<String>,
    start_after: Option<String>,
    limit: Option<u32>,
) -> Result<PoolsResponse, ContractError> {
    let pools = if let Some(pool_identifier) = pool_identifier {
        vec![get_pool(deps, pool_identifier)?]
    } else {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        let start = cw_utils::calc_range_start_string(start_after).map(Bound::ExclusiveRaw);

        POOLS
            .range(deps.storage, start, None, Order::Ascending)
            .take(limit)
            .map(|item| {
                let (_, pool) = item?;
                let total_share = deps.querier.query_supply(&pool.lp_denom)?;

                Ok(PoolInfoResponse {
                    pool_info: pool,
                    total_share,
                })
            })
            .collect::<StdResult<Vec<PoolInfoResponse>>>()?
    };

    Ok(PoolsResponse { pools })
}

/// Gets the pool info for a given pool identifier. Returns a [PoolInfoResponse].
fn get_pool(deps: Deps, pool_identifier: String) -> Result<PoolInfoResponse, ContractError> {
    let pool_info = POOLS.load(deps.storage, &pool_identifier)?;
    let total_share = deps.querier.query_supply(&pool_info.lp_denom)?;

    Ok(PoolInfoResponse {
        pool_info,
        total_share,
    })
}

/// This function iterates over the swap operations, simulates each swap
/// to get the final amount after all the swaps.
pub fn simulate_swap_operations(
    deps: Deps,
    offer_amount: Uint128,
    operations: Vec<SwapOperation>,
) -> Result<SimulateSwapOperationsResponse, ContractError> {
    let operations_len = operations.len();
    ensure!(operations_len > 0, ContractError::NoSwapOperationsProvided);

    let mut amount = offer_amount;
    let mut spreads: Vec<Coin> = vec![];
    let mut swap_fees: Vec<Coin> = vec![];
    let mut protocol_fees: Vec<Coin> = vec![];
    let mut burn_fees: Vec<Coin> = vec![];
    let mut extra_fees: Vec<Coin> = vec![];

    for operation in operations.into_iter() {
        match operation {
            SwapOperation::MantraSwap {
                token_in_denom,
                token_out_denom,
                pool_identifier,
            } => {
                let res = query_simulation(
                    deps,
                    coin(amount.u128(), token_in_denom),
                    token_out_denom.clone(),
                    pool_identifier,
                )?;
                amount = res.return_amount;

                if res.spread_amount > Uint128::zero() {
                    spreads.push(coin(res.spread_amount.u128(), &token_out_denom));
                }
                if res.swap_fee_amount > Uint128::zero() {
                    swap_fees.push(coin(res.swap_fee_amount.u128(), &token_out_denom));
                }
                if res.protocol_fee_amount > Uint128::zero() {
                    protocol_fees.push(coin(res.protocol_fee_amount.u128(), &token_out_denom));
                }
                if res.burn_fee_amount > Uint128::zero() {
                    burn_fees.push(coin(res.burn_fee_amount.u128(), &token_out_denom));
                }
                if res.extra_fees_amount > Uint128::zero() {
                    extra_fees.push(coin(res.extra_fees_amount.u128(), &token_out_denom));
                }
            }
        }
    }

    spreads = aggregate_coins(spreads)?;
    swap_fees = aggregate_coins(swap_fees)?;
    protocol_fees = aggregate_coins(protocol_fees)?;
    burn_fees = aggregate_coins(burn_fees)?;
    extra_fees = aggregate_coins(extra_fees)?;

    Ok(SimulateSwapOperationsResponse {
        return_amount: amount,
        spreads,
        swap_fees,
        protocol_fees,
        burn_fees,
        extra_fees,
    })
}

/// This function iterates over the swap operations in the reverse order,
/// simulates each swap to get the final amount after all the swaps.
pub fn reverse_simulate_swap_operations(
    deps: Deps,
    ask_amount: Uint128,
    operations: Vec<SwapOperation>,
) -> Result<ReverseSimulateSwapOperationsResponse, ContractError> {
    let operations_len = operations.len();
    if operations_len == 0 {
        return Err(ContractError::NoSwapOperationsProvided);
    }

    let mut offer_in_needed = ask_amount;
    let mut spreads: Vec<Coin> = vec![];
    let mut swap_fees: Vec<Coin> = vec![];
    let mut protocol_fees: Vec<Coin> = vec![];
    let mut burn_fees: Vec<Coin> = vec![];
    let mut extra_fees: Vec<Coin> = vec![];

    for operation in operations.into_iter().rev() {
        match operation {
            SwapOperation::MantraSwap {
                token_in_denom,
                token_out_denom,
                pool_identifier,
            } => {
                let res = query_reverse_simulation(
                    deps,
                    coin(offer_in_needed.u128(), token_out_denom.clone()),
                    token_in_denom,
                    pool_identifier,
                )?;

                if res.spread_amount > Uint128::zero() {
                    spreads.push(coin(res.spread_amount.u128(), &token_out_denom));
                }
                if res.swap_fee_amount > Uint128::zero() {
                    swap_fees.push(coin(res.swap_fee_amount.u128(), &token_out_denom));
                }
                if res.protocol_fee_amount > Uint128::zero() {
                    protocol_fees.push(coin(res.protocol_fee_amount.u128(), &token_out_denom));
                }
                if res.burn_fee_amount > Uint128::zero() {
                    burn_fees.push(coin(res.burn_fee_amount.u128(), &token_out_denom));
                }
                if res.extra_fees_amount > Uint128::zero() {
                    extra_fees.push(coin(res.extra_fees_amount.u128(), &token_out_denom));
                }

                offer_in_needed = res.offer_amount;
            }
        }
    }

    spreads = aggregate_coins(spreads)?;
    swap_fees = aggregate_coins(swap_fees)?;
    protocol_fees = aggregate_coins(protocol_fees)?;
    burn_fees = aggregate_coins(burn_fees)?;
    extra_fees = aggregate_coins(extra_fees)?;

    Ok(ReverseSimulateSwapOperationsResponse {
        offer_amount: offer_in_needed,
        spreads,
        swap_fees,
        protocol_fees,
        burn_fees,
        extra_fees,
    })
}
